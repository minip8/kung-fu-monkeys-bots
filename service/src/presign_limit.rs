//! The per-client limit on the routes that hand out presigned URLs
//! (`docs/ladder-api.md` §2, API v1.4): `GET /matches/{m}` (JSON and HTML)
//! and `GET /submissions/{s}`. Each client IP has a token bucket of
//! `KFM_LADDER_PRESIGN_PER_MINUTE` tokens (default 60) that refills at that
//! rate; a request without a token is `429 rate_limited` with `Retry-After`.
//! The client IP is the peer address, or the first `X-Forwarded-For` entry
//! when `KFM_LADDER_TRUST_PROXY=1` (Caddy in front sets it). IPv6 clients
//! share a bucket per /64, the block one host is usually given.
//!
//! The buckets live in this process's memory: several API instances each
//! keep their own, and a restart refills every bucket.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::RETRY_AFTER;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::error::ErrorCode;
use crate::AppState;

/// An idle bucket is full again after a minute, so dropping it changes
/// nothing; the map is swept of those this often.
const PRUNE_EVERY: Duration = Duration::from_secs(60);
const REFILL_WINDOW: Duration = Duration::from_secs(60);

struct Buckets {
    /// Per client: the tokens left and when they were counted.
    map: HashMap<IpAddr, (f64, Instant)>,
    pruned: Instant,
}

/// The token buckets of the presigning routes.
pub struct PresignLimiter {
    per_minute: u32,
    trust_proxy: bool,
    buckets: Mutex<Buckets>,
}

impl PresignLimiter {
    /// `per_minute` requests a minute per client (0: no limit).
    pub fn new(per_minute: u32, trust_proxy: bool) -> PresignLimiter {
        PresignLimiter {
            per_minute,
            trust_proxy,
            buckets: Mutex::new(Buckets {
                map: HashMap::new(),
                pruned: Instant::now(),
            }),
        }
    }

    /// The client a request counts against: the first `X-Forwarded-For`
    /// entry when the proxy is trusted and the entry is an address, else the
    /// peer (unspecified when the server was started without peer
    /// addresses).
    pub fn client_ip(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> IpAddr {
        if self.trust_proxy {
            let forwarded = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .and_then(|first| first.trim().parse::<IpAddr>().ok());
            if let Some(ip) = forwarded {
                return ip;
            }
        }
        peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }

    /// Takes a token of `ip`'s bucket at `now`: `Err` with the whole seconds
    /// until one is back when there is none.
    pub fn take(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let capacity = f64::from(self.per_minute);
        let rate = capacity / REFILL_WINDOW.as_secs_f64();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.saturating_duration_since(buckets.pruned) >= PRUNE_EVERY {
            buckets
                .map
                .retain(|_, (_, last)| now.saturating_duration_since(*last) < REFILL_WINDOW);
            buckets.pruned = now;
        }
        let (tokens, last) = buckets.map.entry(bucket_key(ip)).or_insert((capacity, now));
        let available =
            (*tokens + now.saturating_duration_since(*last).as_secs_f64() * rate).min(capacity);
        *last = now.max(*last);
        if available >= 1.0 {
            *tokens = available - 1.0;
            Ok(())
        } else {
            *tokens = available;
            Err(((1.0 - available) / rate).ceil().max(1.0) as u64)
        }
    }

    /// Clients with a bucket (tests).
    pub fn tracked(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .map
            .len()
    }
}

/// The bucket of an address: itself for IPv4 (an IPv4-mapped IPv6 address
/// is its IPv4 address), its /64 for IPv6.
fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        },
    }
}

/// `429 rate_limited` with `Retry-After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    pub retry_after: u64,
}

impl IntoResponse for RateLimited {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": ErrorCode::RateLimited.as_str(),
            "message": format!(
                "Too many requests for links from your address; try again in {} s.",
                self.retry_after
            ),
        });
        let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(self.retry_after));
        response
    }
}

/// An extractor that takes one token of the caller's bucket: put it first
/// in a presigning route's arguments, so it runs before anything else.
pub struct PresignQuota;

impl FromRequestParts<AppState> for PresignQuota {
    type Rejection = RateLimited;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<PresignQuota, RateLimited> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip());
        let ip = state.presign.client_ip(peer, &parts.headers);
        state
            .presign
            .take(ip, Instant::now())
            .map(|()| PresignQuota)
            .map_err(|retry_after| RateLimited { retry_after })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
    }

    #[test]
    fn the_61st_request_in_a_minute_waits_and_others_do_not() {
        let limiter = PresignLimiter::new(60, false);
        let t0 = Instant::now();
        for _ in 0..60 {
            assert_eq!(limiter.take(v4(1), t0), Ok(()));
        }
        assert_eq!(limiter.take(v4(1), t0), Err(1));
        // Another client has its own bucket.
        assert_eq!(limiter.take(v4(2), t0), Ok(()));
        // One token a second comes back.
        assert_eq!(limiter.take(v4(1), t0 + Duration::from_secs(1)), Ok(()));
        assert_eq!(limiter.take(v4(1), t0 + Duration::from_secs(1)), Err(1));
        // A minute idle refills the bucket, never past its size.
        let later = t0 + Duration::from_secs(600);
        for _ in 0..60 {
            assert_eq!(limiter.take(v4(1), later), Ok(()));
        }
        assert!(limiter.take(v4(1), later).is_err());
    }

    #[test]
    fn retry_after_counts_the_seconds_to_a_token() {
        let limiter = PresignLimiter::new(6, false);
        let t0 = Instant::now();
        for _ in 0..6 {
            limiter.take(v4(1), t0).unwrap();
        }
        // Six a minute: a token every 10 s.
        assert_eq!(limiter.take(v4(1), t0), Err(10));
        assert_eq!(limiter.take(v4(1), t0 + Duration::from_secs(4)), Err(6));
        assert_eq!(limiter.take(v4(1), t0 + Duration::from_secs(11)), Ok(()));
    }

    #[test]
    fn zero_is_no_limit() {
        let limiter = PresignLimiter::new(0, false);
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert_eq!(limiter.take(v4(1), t0), Ok(()));
        }
        assert_eq!(limiter.tracked(), 0);
    }

    #[test]
    fn idle_buckets_are_dropped() {
        let limiter = PresignLimiter::new(60, false);
        let t0 = Instant::now();
        for i in 0..50 {
            limiter.take(v4(i), t0).unwrap();
        }
        assert_eq!(limiter.tracked(), 50);
        limiter.take(v4(200), t0 + PRUNE_EVERY).unwrap();
        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn the_forwarded_address_counts_only_behind_a_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.7, 10.0.0.1".parse().unwrap());
        let local = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let peer = Some(local);
        let untrusted = PresignLimiter::new(60, false);
        assert_eq!(untrusted.client_ip(peer, &headers), local);
        let trusted = PresignLimiter::new(60, true);
        assert_eq!(
            trusted.client_ip(peer, &headers),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
        // Without the header, or with junk in it: the peer.
        assert_eq!(trusted.client_ip(peer, &HeaderMap::new()), local);
        headers.insert("x-forwarded-for", "not an address".parse().unwrap());
        assert_eq!(trusted.client_ip(peer, &headers), local);
        assert_eq!(
            trusted.client_ip(None, &HeaderMap::new()),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn an_ipv6_host_counts_as_its_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(bucket_key(a), bucket_key(b));
        assert_ne!(bucket_key(a), bucket_key(c));
        let mapped: IpAddr = "::ffff:203.0.113.1".parse().unwrap();
        assert_eq!(bucket_key(mapped), v4(1));
        assert_eq!(bucket_key(v4(1)), v4(1));
    }

    #[test]
    fn the_refusal_has_the_error_shape_and_retry_after() {
        let response = RateLimited { retry_after: 7 }.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "7");
    }
}
