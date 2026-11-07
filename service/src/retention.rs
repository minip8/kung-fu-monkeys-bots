//! Retention (architecture §8 "Submissions and retention"): a team keeps the
//! source and log of its [`KEEP`] most recent finished submissions (`ready`,
//! `failed`, `rejected`) and of its active one; older ones lose both objects
//! and their keys. A module is never deleted (every recorded match stays
//! reproducible from seed plus modules), queued and building submissions are
//! never touched, and the row itself stays.

use std::collections::HashSet;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::store::Store;

/// Finished submissions kept per team, besides the active one.
pub const KEEP: usize = 5;

/// The finished submissions to prune: all but the `KEEP` most recent (by
/// creation, then id) and never `active`.
pub fn to_prune(finished: &[(Uuid, OffsetDateTime)], active: Option<Uuid>) -> Vec<Uuid> {
    let mut sorted = finished.to_vec();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    sorted
        .into_iter()
        .skip(KEEP)
        .map(|(id, _)| id)
        .filter(|id| Some(*id) != active)
        .collect()
}

/// Prunes `team`: clears the keys of what [`to_prune`] picks (under the
/// team's row lock, which activation takes too, so the active one cannot
/// change underneath), then deletes the objects the keys named. Returns the
/// number of submissions pruned. A failed delete leaves an orphan object,
/// logged; the rows never name a deleted object.
pub async fn prune_team(db: &PgPool, store: &Store, team: Uuid) -> Result<usize, String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("SELECT 1 FROM teams WHERE id = $1 FOR UPDATE")
        .bind(team)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let finished: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, created_at FROM submissions \
         WHERE team_id = $1 AND state IN ('ready', 'failed', 'rejected')",
    )
    .bind(team)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let active: Option<(Uuid,)> =
        sqlx::query_as("SELECT submission_id FROM active_bots WHERE team_id = $1")
            .bind(team)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    let prune = to_prune(&finished, active.map(|(id,)| id));
    if prune.is_empty() {
        return Ok(0);
    }
    let cleared: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "WITH old AS (SELECT id, source_key, log_key FROM submissions \
                      WHERE id = ANY($1) AND (source_key IS NOT NULL OR log_key IS NOT NULL)) \
         UPDATE submissions s SET source_key = NULL, log_key = NULL, updated_at = now() \
         FROM old WHERE s.id = old.id RETURNING s.id, old.source_key, old.log_key",
    )
    .bind(&prune)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;

    let keys: HashSet<String> = cleared
        .iter()
        .flat_map(|(_, source, log)| [source.clone(), log.clone()])
        .flatten()
        .collect();
    for key in &keys {
        debug_assert!(!key.starts_with("modules/"));
        if let Err(err) = store.delete(key).await {
            tracing::warn!(%key, error = %err, "pruning left an object behind");
        }
    }
    if !cleared.is_empty() {
        tracing::info!(%team, pruned = cleared.len(), objects = keys.len(), "retention pruned submissions");
    }
    Ok(cleared.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(minute)
    }

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|i| Uuid::from_u128(i as u128 + 1)).collect()
    }

    #[test]
    fn the_five_most_recent_are_kept() {
        let ids = ids(8);
        let finished: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, at(i as i64)))
            .collect();
        let mut pruned = to_prune(&finished, None);
        pruned.sort();
        assert_eq!(pruned, ids[..3].to_vec());
        assert!(to_prune(&finished[..5], None).is_empty());
        assert!(to_prune(&[], None).is_empty());
    }

    #[test]
    fn the_active_one_is_never_pruned_and_does_not_count_twice() {
        let ids = ids(7);
        let finished: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, at(i as i64)))
            .collect();
        // The oldest is active: it stays, beside the five most recent.
        assert_eq!(to_prune(&finished, Some(ids[0])), vec![ids[1]]);
        // A recent active one is just one of the five.
        let mut pruned = to_prune(&finished, Some(ids[6]));
        pruned.sort();
        assert_eq!(pruned, ids[..2].to_vec());
    }

    #[test]
    fn order_is_by_creation_then_id_whatever_the_input_order() {
        let ids = ids(6);
        let mut finished: Vec<_> = ids.iter().map(|id| (*id, at(0))).collect();
        finished.reverse();
        // All at one instant: the smallest id is the oldest.
        assert_eq!(to_prune(&finished, None), vec![ids[0]]);
    }
}
