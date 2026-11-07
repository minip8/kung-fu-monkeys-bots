//! Glicko-2 (Mark E. Glickman, "Example of the Glicko-2 system", 2013): the
//! ladder's rating (architecture §8 "Matches and ratings", v0.63). Ratings
//! are kept on Glicko-2's own scale (`mu` around 0, `phi` the deviation,
//! `sigma` the volatility) and shown on Glicko's (`r = 1500 + 173.7178 mu`,
//! `RD = 173.7178 phi`).
//!
//! The ladder applies each match as a rating period of one game
//! ([`rate`] with a one-element slice), updating both teams against each
//! other's rating from before the match; a period batched per round is a
//! later refinement. At an engine release a team's deviation is widened by
//! [`widen`] before its first match under the new release (v0.59): the
//! release is a shock shared by every entrant, so the ratings persist and
//! only their certainty drops.

use std::f64::consts::PI;

/// Glicko-2's scale factor, 400 / ln 10.
pub const SCALE: f64 = 173.7178;
/// A new team's rating, deviation and volatility (Glickman's defaults).
pub const DEFAULT_RATING: f64 = 1500.0;
pub const DEFAULT_RD: f64 = 350.0;
pub const DEFAULT_SIGMA: f64 = 0.06;
/// The system constant: how much the volatility may move (Glickman suggests
/// 0.3 to 1.2; 0.5 is the paper's example).
pub const TAU: f64 = 0.5;
/// The volatility iteration's convergence tolerance (the paper's).
pub const EPSILON: f64 = 0.000_001;
/// The deviation an engine release adds, in quadrature, on Glicko's scale:
/// RD' = min(350, sqrt(RD² + 100²)). A settled team (RD 50) goes to 112, an
/// unsettled one (RD 300) to 316: a release costs a settled team a few
/// matches of certainty, never its rating.
pub const RELEASE_RD: f64 = 100.0;

/// A rating on Glicko-2's scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rating {
    pub mu: f64,
    pub phi: f64,
    pub sigma: f64,
}

impl Default for Rating {
    fn default() -> Rating {
        Rating::from_glicko(DEFAULT_RATING, DEFAULT_RD, DEFAULT_SIGMA)
    }
}

impl Rating {
    /// From Glicko's scale (`r`, `RD`).
    pub fn from_glicko(rating: f64, rd: f64, sigma: f64) -> Rating {
        Rating {
            mu: (rating - DEFAULT_RATING) / SCALE,
            phi: rd / SCALE,
            sigma,
        }
    }

    /// The rating on Glicko's scale.
    pub fn rating(&self) -> f64 {
        DEFAULT_RATING + SCALE * self.mu
    }

    /// The deviation on Glicko's scale.
    pub fn rd(&self) -> f64 {
        SCALE * self.phi
    }
}

/// One game of a rating period: the opponent's rating before the period and
/// the score, 1 a win, 0.5 a draw, 0 a loss.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Game {
    pub opponent: Rating,
    pub score: f64,
}

fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi * phi / (PI * PI)).sqrt()
}

fn expected(mu: f64, mu_j: f64, phi_j: f64) -> f64 {
    1.0 / (1.0 + (-g(phi_j) * (mu - mu_j)).exp())
}

/// The new volatility (step 5, the Illinois algorithm).
fn volatility(phi: f64, sigma: f64, v: f64, delta: f64, tau: f64) -> f64 {
    let a = (sigma * sigma).ln();
    let f = |x: f64| {
        let ex = x.exp();
        let denom = phi * phi + v + ex;
        ex * (delta * delta - phi * phi - v - ex) / (2.0 * denom * denom) - (x - a) / (tau * tau)
    };
    let mut big_a = a;
    let mut big_b = if delta * delta > phi * phi + v {
        (delta * delta - phi * phi - v).ln()
    } else {
        let mut k = 1.0;
        while f(a - k * tau) < 0.0 {
            k += 1.0;
        }
        a - k * tau
    };
    let mut f_a = f(big_a);
    let mut f_b = f(big_b);
    // Converges in a handful of steps; the cap only guards a NaN input.
    for _ in 0..1000 {
        if (big_b - big_a).abs() <= EPSILON {
            break;
        }
        let c = big_a + (big_a - big_b) * f_a / (f_b - f_a);
        let f_c = f(c);
        if f_c * f_b <= 0.0 {
            big_a = big_b;
            f_a = f_b;
        } else {
            f_a /= 2.0;
        }
        big_b = c;
        f_b = f_c;
    }
    (big_a / 2.0).exp()
}

/// The player's rating after a rating period of `games` (Glickman's steps 3
/// to 7), with the system constant `tau`. A period without games only
/// widens the deviation by the volatility.
pub fn rate(player: Rating, games: &[Game], tau: f64) -> Rating {
    if games.is_empty() {
        return Rating {
            phi: (player.phi * player.phi + player.sigma * player.sigma).sqrt(),
            ..player
        };
    }
    let mut v_inv = 0.0;
    let mut sum = 0.0;
    for game in games {
        let g_j = g(game.opponent.phi);
        let e = expected(player.mu, game.opponent.mu, game.opponent.phi);
        v_inv += g_j * g_j * e * (1.0 - e);
        sum += g_j * (game.score - e);
    }
    let v = 1.0 / v_inv;
    let delta = v * sum;
    let sigma = volatility(player.phi, player.sigma, v, delta, tau);
    let phi_star = (player.phi * player.phi + sigma * sigma).sqrt();
    let phi = 1.0 / (1.0 / (phi_star * phi_star) + 1.0 / v).sqrt();
    Rating {
        mu: player.mu + phi * phi * sum,
        phi,
        sigma,
    }
}

/// The rating at an engine release: the deviation widened by
/// [`RELEASE_RD`] in quadrature, never past [`DEFAULT_RD`].
pub fn widen(rating: Rating) -> Rating {
    let rd = (rating.rd() * rating.rd() + RELEASE_RD * RELEASE_RD)
        .sqrt()
        .min(DEFAULT_RD);
    Rating {
        phi: rd / SCALE,
        ..rating
    }
}

/// The two sides of one game after it (each rated against the other's
/// rating from before): `score_a` is side A's score.
pub fn rate_pair(a: Rating, b: Rating, score_a: f64) -> (Rating, Rating) {
    (
        rate(
            a,
            &[Game {
                opponent: b,
                score: score_a,
            }],
            TAU,
        ),
        rate(
            b,
            &[Game {
                opponent: a,
                score: 1.0 - score_a,
            }],
            TAU,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    /// Glickman's worked example: a 1500/200/0.06 player beats a 1400/30,
    /// loses to a 1550/100 and to a 1700/300, in one period, tau 0.5.
    #[test]
    fn the_papers_example() {
        let player = Rating::from_glicko(1500.0, 200.0, 0.06);
        let opponent = |r, rd| Rating::from_glicko(r, rd, DEFAULT_SIGMA);
        let games = [
            Game {
                opponent: opponent(1400.0, 30.0),
                score: 1.0,
            },
            Game {
                opponent: opponent(1550.0, 100.0),
                score: 0.0,
            },
            Game {
                opponent: opponent(1700.0, 300.0),
                score: 0.0,
            },
        ];
        let after = rate(player, &games, 0.5);
        // The paper's figures: mu' = -0.2069, phi' = 0.8722, sigma' = 0.05999,
        // r' = 1464.06, RD' = 151.52.
        assert!(close(after.mu, -0.2069, 0.0001), "{after:?}");
        assert!(close(after.phi, 0.8722, 0.0001), "{after:?}");
        assert!(close(after.sigma, 0.05999, 0.00001), "{after:?}");
        assert!(close(after.rating(), 1464.06, 0.01), "{}", after.rating());
        assert!(close(after.rd(), 151.52, 0.01), "{}", after.rd());
    }

    #[test]
    fn the_scales_round_trip() {
        let r = Rating::from_glicko(1612.5, 87.25, 0.059);
        assert!(close(r.rating(), 1612.5, 1e-9));
        assert!(close(r.rd(), 87.25, 1e-9));
        let d = Rating::default();
        assert_eq!((d.mu, d.sigma), (0.0, DEFAULT_SIGMA));
        assert!(close(d.rd(), 350.0, 1e-9));
    }

    #[test]
    fn a_period_without_games_widens_by_the_volatility() {
        let r = Rating::from_glicko(1500.0, 200.0, 0.06);
        let after = rate(r, &[], TAU);
        assert_eq!(after.mu, r.mu);
        assert!(close(after.phi, (r.phi * r.phi + 0.0036).sqrt(), 1e-12));
    }

    #[test]
    fn one_game_moves_both_sides_symmetrically() {
        let (a, b) = rate_pair(Rating::default(), Rating::default(), 1.0);
        assert!(a.rating() > 1500.0 && b.rating() < 1500.0);
        assert!(close(a.rating() - 1500.0, 1500.0 - b.rating(), 1e-9));
        assert!(a.rd() < 350.0 && close(a.rd(), b.rd(), 1e-9));
        // A draw between equals moves neither rating, only the deviation.
        let (c, d) = rate_pair(Rating::default(), Rating::default(), 0.5);
        assert!(close(c.rating(), 1500.0, 1e-9) && close(d.rating(), 1500.0, 1e-9));
        assert!(c.rd() < 350.0);
        // Beating a much weaker side gains little; losing to it costs a lot.
        let strong = Rating::from_glicko(1800.0, 60.0, 0.06);
        let weak = Rating::from_glicko(1400.0, 60.0, 0.06);
        let (won, _) = rate_pair(strong, weak, 1.0);
        let (lost, _) = rate_pair(strong, weak, 0.0);
        assert!(won.rating() - 1800.0 < 1800.0 - lost.rating());
    }

    #[test]
    fn a_release_widens_the_deviation_up_to_the_default() {
        let settled = Rating::from_glicko(1700.0, 50.0, 0.06);
        let widened = widen(settled);
        assert!(close(
            widened.rd(),
            (50.0f64 * 50.0 + 100.0 * 100.0).sqrt(),
            1e-9
        ));
        assert!(close(widened.rd(), 111.80, 0.01));
        assert_eq!(widened.mu, settled.mu);
        assert_eq!(widened.sigma, settled.sigma);
        let unsettled = Rating::from_glicko(1500.0, 340.0, 0.06);
        assert!(close(widen(unsettled).rd(), 350.0, 1e-9));
        assert!(close(widen(Rating::default()).rd(), 350.0, 1e-9));
    }
}
