//! The scheduler (architecture §11 "Scheduling and ratings"): Swiss-style
//! rounds in the current season. A round takes every team whose active
//! submission is `ready`, sorted by its rating in the season (a team not
//! yet rated is at 1500), and pairs each with up to k opponents nearest to
//! it by rating that it has not met in this round: k passes of a greedy
//! walk from the top ([`pair`]). A team takes part in at most k pairs, so a
//! round of n teams has at most n·k/2. A pair whose match is still queued or
//! running counts as paired (it will produce a result) but gets no new job.
//! Each new pair is one `match` job, played from both sides through
//! `--rotate`, with a seed derived from the season, the round number and the
//! two submissions.
//!
//! `kfm-ladder serve` runs [`run`] in the background: it holds a Postgres
//! session advisory lock ([`LOCK_KEY`], `pg_try_advisory_lock`) on a
//! connection of its own and makes rounds over that connection only, so of
//! several API instances one schedules and the others stay idle until its
//! connection goes. A round is due once the season's last round is
//! `KFM_LADDER_ROUND_SECONDS` old (judged on the database's clock), so a
//! restart neither skips nor doubles one. Nothing is scheduled outside a
//! season, and fewer than two eligible teams make no round.

use std::collections::HashSet;
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use crate::glicko;
use crate::jobs::{self, Kind};
use crate::seasons::Season;

/// The advisory lock the scheduling instance holds ("kfm_ladd" in ASCII).
pub const LOCK_KEY: i64 = 0x6b66_6d5f_6c61_6464;
pub const DEFAULT_ROUND_SECONDS: u64 = 300;
pub const DEFAULT_OPPONENTS: usize = 3;
/// The longest the loop sleeps between two looks at whether a round is due.
pub const MAX_POLL: Duration = Duration::from_secs(15);

/// The scheduler's settings.
#[derive(Debug, Clone)]
pub struct SchedulerSettings {
    /// `KFM_LADDER_ROUND_SECONDS`: a round is due when the last one is this old.
    pub every: Duration,
    /// `KFM_LADDER_ROUND_OPPONENTS`: k, the most pairs a team is in per round.
    pub opponents: usize,
    /// How often the loop looks (at most [`MAX_POLL`]).
    pub poll: Duration,
}

impl SchedulerSettings {
    pub fn new(every: Duration, opponents: usize) -> SchedulerSettings {
        SchedulerSettings {
            every,
            opponents,
            poll: every.clamp(Duration::from_millis(50), MAX_POLL),
        }
    }
}

/// A team in a round: its active submission and its rating.
#[derive(Debug, Clone, PartialEq)]
pub struct Entrant {
    pub team: Uuid,
    pub submission: Uuid,
    /// The rating on Glicko's scale (1500 for a team not yet rated).
    pub rating: f64,
}

/// Sorts a round's entrants: the higher rating first, then the team id.
pub fn sort_entrants(entrants: &mut [Entrant]) {
    entrants.sort_by(|a, b| b.rating.total_cmp(&a.rating).then(a.team.cmp(&b.team)));
}

fn unordered(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// The pairs of a round over `entrants` (sorted by [`sort_entrants`]), as
/// index pairs `(i, j)` with `i < j` (side A the higher ranked), in the
/// order made. The round is k passes; in each pass every team is in at most
/// one new pair, and the walk goes from the top in rank order, except that
/// teams with fewer pairs so far walk first (so the team an odd pass left
/// out leads the next). Walking, a team without a pair in this pass takes
/// the nearest team by rating that is also free in this pass, has fewer
/// than k pairs and has not met it in this round; ties go to the higher
/// ranked. `busy` holds team pairs (either order) whose match is still
/// queued or running: they are made and counted like any other but not
/// returned.
pub fn pair(entrants: &[Entrant], k: usize, busy: &HashSet<(Uuid, Uuid)>) -> Vec<(usize, usize)> {
    let n = entrants.len();
    let mut count = vec![0usize; n];
    let mut met: HashSet<(usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    for _pass in 0..k {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| (count[i], i));
        let mut taken = vec![false; n];
        let mut made_any = false;
        for i in order {
            if taken[i] || count[i] >= k {
                continue;
            }
            let nearest = (0..n)
                .filter(|&j| {
                    j != i && !taken[j] && count[j] < k && !met.contains(&(i.min(j), i.max(j)))
                })
                .min_by(|&x, &y| {
                    let dx = (entrants[x].rating - entrants[i].rating).abs();
                    let dy = (entrants[y].rating - entrants[i].rating).abs();
                    dx.total_cmp(&dy).then(x.cmp(&y))
                });
            let Some(j) = nearest else { continue };
            let (lo, hi) = (i.min(j), i.max(j));
            met.insert((lo, hi));
            taken[i] = true;
            taken[j] = true;
            count[i] += 1;
            count[j] += 1;
            made_any = true;
            if !busy.contains(&unordered(entrants[lo].team, entrants[hi].team)) {
                out.push((lo, hi));
            }
        }
        if !made_any {
            break;
        }
    }
    out
}

/// A match's seed: the first 8 bytes of SHA-256 over a fixed label, the
/// season, the round number and the two submissions (side A's first), as a
/// big-endian integer shifted right by 2, so it is below 2^62 and the
/// second game's seed (`--seed` + 1 in kfm_arena) still fits a bigint.
pub fn seed(season: Uuid, round: i32, submission_a: Uuid, submission_b: Uuid) -> i64 {
    let mut hash = Sha256::new();
    hash.update(b"kfm-ladder match seed v1\0");
    hash.update(season.as_bytes());
    hash.update(round.to_be_bytes());
    hash.update(submission_a.as_bytes());
    hash.update(submission_b.as_bytes());
    let digest = hash.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    // < 2^62 by the shift, so the cast is exact.
    (u64::from_be_bytes(first) >> 2) as i64
}

/// One pair a round queued.
#[derive(Debug, Clone, PartialEq)]
pub struct Scheduled {
    pub job: Uuid,
    pub a: Entrant,
    pub b: Entrant,
    pub seed: i64,
}

/// What a scheduler tick did.
#[derive(Debug, Clone, PartialEq)]
pub enum RoundOutcome {
    /// No season holds now: nothing is scheduled.
    NoSeason,
    /// The season's last round is younger than the round interval.
    NotDue,
    /// Fewer than two teams have an active ready submission: no round.
    TooFew { season: Uuid, teams: usize },
    /// A round was made (possibly with every pair busy).
    Ran {
        season: Uuid,
        number: i32,
        teams: usize,
        pairs: Vec<Scheduled>,
        busy: usize,
    },
}

/// The payload of a `match` job.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MatchPayload {
    pub season: Uuid,
    pub round: i32,
    pub mode: String,
    pub team_a: Uuid,
    pub submission_a: Uuid,
    pub team_b: Uuid,
    pub submission_b: Uuid,
    pub seed: i64,
}

/// Makes one round in the current season over `conn`, if one is due: a
/// round is due when the season has none yet or its last is at least
/// `every` old. The season's row is locked for the round, so two callers
/// never number a round twice.
pub async fn run_round(
    conn: &mut PgConnection,
    every: Duration,
    opponents: usize,
) -> Result<RoundOutcome, sqlx::Error> {
    let mut tx = conn.begin().await?;
    let season: Option<Season> = sqlx::query_as(
        "SELECT id, name, mode, starts_at, ends_at FROM seasons \
         WHERE starts_at <= now() AND now() < ends_at FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(season) = season else {
        return Ok(RoundOutcome::NoSeason);
    };
    let (recent,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM rounds WHERE season_id = $1 \
                        AND created_at > now() - make_interval(secs => $2))",
    )
    .bind(season.id)
    .bind(every.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;
    if recent {
        return Ok(RoundOutcome::NotDue);
    }
    let rows: Vec<(Uuid, Uuid, Option<f64>)> = sqlx::query_as(
        "SELECT a.team_id, a.submission_id, r.mu FROM active_bots a \
         JOIN submissions s ON s.id = a.submission_id AND s.state = 'ready' AND s.module_key IS NOT NULL \
         LEFT JOIN ratings r ON r.season_id = $1 AND r.team_id = a.team_id",
    )
    .bind(season.id)
    .fetch_all(&mut *tx)
    .await?;
    let mut entrants: Vec<Entrant> = rows
        .into_iter()
        .map(|(team, submission, mu)| Entrant {
            team,
            submission,
            rating: glicko::DEFAULT_RATING + glicko::SCALE * mu.unwrap_or(0.0),
        })
        .collect();
    if entrants.len() < 2 {
        return Ok(RoundOutcome::TooFew {
            season: season.id,
            teams: entrants.len(),
        });
    }
    sort_entrants(&mut entrants);
    let live: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT payload->>'team_a', payload->>'team_b' FROM jobs \
         WHERE kind = 'match' AND state IN ('queued', 'running')",
    )
    .fetch_all(&mut *tx)
    .await?;
    let busy: HashSet<(Uuid, Uuid)> = live
        .into_iter()
        .filter_map(|(a, b)| {
            let a = Uuid::parse_str(&a?).ok()?;
            let b = Uuid::parse_str(&b?).ok()?;
            Some(unordered(a, b))
        })
        .collect();
    let made = pair(&entrants, opponents, &busy);
    let teams = entrants.len();
    let met_busy = {
        // Busy pairs among this round's teams, for the record.
        let present: HashSet<Uuid> = entrants.iter().map(|e| e.team).collect();
        busy.iter()
            .filter(|(a, b)| present.contains(a) && present.contains(b))
            .count()
    };
    let (number,): (i32,) =
        sqlx::query_as("SELECT coalesce(max(number), 0) + 1 FROM rounds WHERE season_id = $1")
            .bind(season.id)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query(
        "INSERT INTO rounds (season_id, number, teams, pairs, busy) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(season.id)
    .bind(number)
    .bind(i32::try_from(teams).unwrap_or(i32::MAX))
    .bind(i32::try_from(made.len()).unwrap_or(i32::MAX))
    .bind(i32::try_from(met_busy).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    let mut pairs = Vec::new();
    for (i, j) in made {
        let (a, b) = (entrants[i].clone(), entrants[j].clone());
        let seed = seed(season.id, number, a.submission, b.submission);
        let payload = MatchPayload {
            season: season.id,
            round: number,
            mode: season.mode.clone(),
            team_a: a.team,
            submission_a: a.submission,
            team_b: b.team,
            submission_b: b.submission,
            seed,
        };
        let job = jobs::enqueue(
            &mut tx,
            Kind::Match,
            serde_json::to_value(&payload).expect("a payload serialises"),
        )
        .await?;
        pairs.push(Scheduled { job, a, b, seed });
    }
    tx.commit().await?;
    tracing::info!(season = %season.name, round = number, teams, pairs = pairs.len(), busy = met_busy, "round scheduled");
    Ok(RoundOutcome::Ran {
        season: season.id,
        number,
        teams,
        pairs,
        busy: met_busy,
    })
}

/// Tries to take the scheduler's advisory lock on `conn` (held until the
/// connection closes or [`unlock`]).
pub async fn try_lock(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    let (held,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
        .bind(LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    Ok(held)
}

pub async fn unlock(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    let (released,): (bool,) = sqlx::query_as("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    Ok(released)
}

/// The scheduler loop, until `shutdown` turns true: take the lock on a
/// connection detached from the pool (a pooled connection holding it would
/// be handed to other queries), then make a round whenever one is due. A
/// failed round drops the connection, and with it the lock; the next look
/// starts over.
pub async fn run(db: PgPool, settings: SchedulerSettings, mut shutdown: watch::Receiver<bool>) {
    tracing::info!(every = ?settings.every, opponents = settings.opponents, "scheduler started");
    let mut held: Option<PgConnection> = None;
    let mut waiting_logged = false;
    while !*shutdown.borrow() {
        if held.is_none() {
            match db.acquire().await {
                Ok(pooled) => {
                    let mut conn = pooled.detach();
                    match try_lock(&mut conn).await {
                        Ok(true) => {
                            tracing::info!("scheduler: holding the lock; this instance schedules");
                            waiting_logged = false;
                            held = Some(conn);
                        }
                        Ok(false) => {
                            if !waiting_logged {
                                tracing::info!("scheduler: another instance holds the lock; idle");
                                waiting_logged = true;
                            }
                            let _ = conn.close().await;
                        }
                        Err(err) => tracing::warn!(error = %err, "scheduler: cannot take the lock"),
                    }
                }
                Err(err) => tracing::warn!(error = %err, "scheduler: no database connection"),
            }
        }
        if let Some(conn) = held.as_mut() {
            match run_round(conn, settings.every, settings.opponents).await {
                Ok(RoundOutcome::TooFew { teams, .. }) => {
                    tracing::debug!(teams, "scheduler: fewer than two teams; no round");
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "scheduler: the round failed; letting the lock go");
                    if let Some(conn) = held.take() {
                        let _ = conn.close().await;
                    }
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(settings.poll) => {}
            _ = shutdown.changed() => {}
        }
    }
    if let Some(mut conn) = held.take() {
        let _ = unlock(&mut conn).await;
        let _ = conn.close().await;
    }
    tracing::info!("scheduler stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entrants(ratings: &[f64]) -> Vec<Entrant> {
        let mut out: Vec<Entrant> = ratings
            .iter()
            .enumerate()
            .map(|(i, &rating)| Entrant {
                team: Uuid::from_u128(i as u128 + 1),
                submission: Uuid::from_u128(1000 + i as u128),
                rating,
            })
            .collect();
        sort_entrants(&mut out);
        out
    }

    fn degrees(n: usize, pairs: &[(usize, usize)]) -> Vec<usize> {
        let mut d = vec![0; n];
        for &(a, b) in pairs {
            d[a] += 1;
            d[b] += 1;
        }
        d
    }

    #[test]
    fn entrants_sort_by_rating_then_team() {
        let mut list = entrants(&[1500.0, 1700.0, 1500.0, 1300.0]);
        sort_entrants(&mut list);
        let order: Vec<u128> = list.iter().map(|e| e.team.as_u128()).collect();
        assert_eq!(order, [2, 1, 3, 4]);
    }

    #[test]
    fn each_team_meets_its_nearest_neighbours_from_the_top() {
        // Ratings far apart in two clusters: each pairs within its cluster.
        let list = entrants(&[2000.0, 1990.0, 1980.0, 1000.0, 990.0, 980.0]);
        // k = 1: a perfect matching of neighbours from the top.
        assert_eq!(pair(&list, 1, &HashSet::new()), [(0, 1), (2, 3), (4, 5)]);
        // k = 2: the second pass walks from the top again, each team taking
        // its nearest free neighbour it has not met (4 and 5 have met).
        let pairs = pair(&list, 2, &HashSet::new());
        assert_eq!(pairs, [(0, 1), (2, 3), (4, 5), (0, 2), (1, 3)]);
    }

    #[test]
    fn no_team_exceeds_k_and_no_pair_repeats() {
        for n in 0..=13 {
            for k in 1..=4 {
                let ratings: Vec<f64> = (0..n).map(|i| 1500.0 + 37.0 * i as f64 % 211.0).collect();
                let list = entrants(&ratings);
                let pairs = pair(&list, k, &HashSet::new());
                let unique: HashSet<_> = pairs.iter().collect();
                assert_eq!(unique.len(), pairs.len(), "n {n} k {k}");
                assert!(pairs.iter().all(|&(a, b)| a < b && b < n));
                assert!(degrees(n, &pairs).iter().all(|&d| d <= k), "n {n} k {k}");
                assert!(pairs.len() <= n * k / 2, "n {n} k {k}");
                // Everyone plays once there are two teams, except the one
                // team an odd count leaves out of a single pass (k = 1).
                if n >= 2 {
                    let d = degrees(n, &pairs);
                    let idle = d.iter().filter(|&&x| x == 0).count();
                    let allowed = usize::from(k == 1 && n % 2 == 1);
                    assert!(idle <= allowed, "n {n} k {k}: {d:?}");
                    // Greedy, so not always every possible pair, but close.
                    let ideal = n * k.min(n - 1) / 2;
                    assert!(
                        pairs.len() * 3 >= ideal * 2,
                        "n {n} k {k}: {} of {ideal}",
                        pairs.len()
                    );
                }
            }
        }
    }

    #[test]
    fn odd_counts_and_too_few() {
        assert!(pair(&entrants(&[]), 3, &HashSet::new()).is_empty());
        assert!(pair(&entrants(&[1500.0]), 3, &HashSet::new()).is_empty());
        assert_eq!(
            pair(&entrants(&[1500.0, 1400.0]), 3, &HashSet::new()),
            [(0, 1)]
        );
        // Three teams, k = 3: everyone meets everyone once (n - 1 = 2 < k).
        assert_eq!(
            pair(&entrants(&[1600.0, 1500.0, 1400.0]), 3, &HashSet::new()),
            [(0, 1), (1, 2), (0, 2)]
        );
        // Five teams, k = 1: two pairs, the last team sits out.
        let five = pair(
            &entrants(&[1900.0, 1800.0, 1700.0, 1600.0, 1500.0]),
            1,
            &HashSet::new(),
        );
        assert_eq!(five, [(0, 1), (2, 3)]);
    }

    #[test]
    fn a_busy_pair_counts_but_is_not_queued() {
        let list = entrants(&[1800.0, 1700.0, 1600.0, 1500.0]);
        let busy: HashSet<_> = [unordered(list[1].team, list[0].team)].into();
        // k = 1: 0-1 is busy (counted, not returned), so 2-3 is the only new pair.
        assert_eq!(pair(&list, 1, &busy), [(2, 3)]);
        // k = 2: 0 and 1 still each take one more neighbour.
        assert_eq!(pair(&list, 2, &busy), [(2, 3), (0, 2), (1, 3)]);
    }

    #[test]
    fn ties_go_to_the_higher_ranked_neighbour() {
        // Team 1 is equally far from 0 and 2; with k = 1, 0 takes 1 first.
        let list = entrants(&[1600.0, 1500.0, 1400.0]);
        assert_eq!(pair(&list, 1, &HashSet::new()), [(0, 1)]);
    }

    #[test]
    fn seeds_are_deterministic_below_2_62_and_depend_on_every_input() {
        let (s, a, b) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let base = seed(s, 1, a, b);
        assert_eq!(base, seed(s, 1, a, b));
        assert!((0..1i64 << 62).contains(&base));
        for other in [
            seed(Uuid::from_u128(9), 1, a, b),
            seed(s, 2, a, b),
            seed(s, 1, b, a),
            seed(s, 1, a, Uuid::from_u128(4)),
        ] {
            assert_ne!(other, base);
        }
        for round in 0..2000 {
            let x = seed(s, round, a, b);
            assert!((0..(1i64 << 62)).contains(&x));
            assert!(x.checked_add(1).is_some());
        }
    }
}
