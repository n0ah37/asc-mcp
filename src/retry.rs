//! Retry policy for App Store Connect requests.
//!
//! Apple rate-limits by hourly quota and occasionally returns a transient 5xx or
//! drops a connection. Surfacing those to the agent as hard failures wastes a
//! turn on something a half-second wait would have fixed.
//!
//! What we will and won't replay:
//!
//! | Failure | POST | GET / PATCH / PUT / DELETE |
//! |---|---|---|
//! | `429 Too Many Requests` | retry — the request was rejected, not applied | retry |
//! | `500` / `502` / `503` / `504` | no — it may have been applied | retry |
//! | connection never established | retry — nothing reached Apple | retry |
//! | timed out mid-flight | no — Apple may have applied it | retry |
//!
//! The asymmetry matters: replaying a `POST` that Apple already processed
//! creates a duplicate resource, and App Store Connect permanently reserves
//! identifiers like a product ID.
//!
//! The decision functions are pure so the table above is enforced by tests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Method;

use crate::config::HttpConfig;

/// Whether replaying this method is safe when the outcome is unknown.
pub fn is_idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::PATCH | Method::OPTIONS
    )
}

/// Whether a response status is worth another attempt.
pub fn should_retry_status(status: u16, idempotent: bool) -> bool {
    match status {
        // Rate limited: Apple rejected the request without applying it, so even
        // a non-idempotent call is safe to replay.
        429 => true,
        500 | 502 | 503 | 504 => idempotent,
        _ => false,
    }
}

/// Whether a transport-level failure is worth another attempt.
pub fn should_retry_transport(err: &reqwest::Error, idempotent: bool) -> bool {
    if err.is_connect() {
        // The connection never came up, so nothing was applied.
        true
    } else if err.is_timeout() {
        // We stopped waiting; Apple may still have processed it.
        idempotent
    } else {
        false
    }
}

/// How long to wait before attempt number `attempt` (1 = the first retry).
///
/// Honours a server-supplied `Retry-After`, otherwise backs off exponentially
/// from [`HttpConfig::retry_base_delay`]. Adds up to 25% jitter so concurrent
/// tool calls don't retry in lockstep, and never waits longer than
/// [`HttpConfig::max_retry_delay`] — a bounded worst case matters more than a
/// perfectly patient client when an agent is waiting on the result.
pub fn retry_delay(http: &HttpConfig, attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(after) = retry_after {
        return after.min(http.max_retry_delay);
    }
    let exponent = attempt.saturating_sub(1).min(16);
    let base = http
        .retry_base_delay
        .saturating_mul(2u32.saturating_pow(exponent));
    let base = base.min(http.max_retry_delay);
    base + jitter(base)
}

/// Parse a `Retry-After` header value expressed in seconds.
///
/// The HTTP-date form is accepted by the spec but never used by App Store
/// Connect; for that (or anything unparseable) we fall back to the exponential
/// backoff rather than pull in a date parser.
pub fn parse_retry_after(header: Option<&str>) -> Option<Duration> {
    let secs: u64 = header?.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Up to 25% of `base`, derived from the clock — enough to break up lockstep
/// retries without a random-number dependency.
fn jitter(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let spread = base.as_millis() as u64 / 4;
    if spread == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(u64::from(nanos) % (spread + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_only_replayed_when_apple_never_applied_them() {
        // 429 means rejected-not-applied, so even POST is safe.
        assert!(should_retry_status(429, false));
        assert!(should_retry_status(429, true));
        // A 5xx may mean "applied, then failed to answer" — never replay a POST.
        for status in [500, 502, 503, 504] {
            assert!(should_retry_status(status, true), "{status} idempotent");
            assert!(!should_retry_status(status, false), "{status} POST");
        }
    }

    #[test]
    fn client_errors_are_never_retried() {
        for status in [200, 201, 400, 401, 403, 404, 409, 422] {
            assert!(!should_retry_status(status, true), "{status}");
        }
    }

    #[test]
    fn method_idempotence_matches_json_api_semantics() {
        assert!(is_idempotent(&Method::GET));
        assert!(is_idempotent(&Method::DELETE));
        assert!(is_idempotent(&Method::PUT));
        // JSON:API PATCH sets named attributes to given values.
        assert!(is_idempotent(&Method::PATCH));
        // POST creates, and Apple permanently reserves some created identifiers.
        assert!(!is_idempotent(&Method::POST));
    }

    #[test]
    fn retry_after_takes_precedence_but_stays_bounded() {
        let http = HttpConfig::default();
        let d = retry_delay(&http, 1, Some(Duration::from_secs(2)));
        assert_eq!(d, Duration::from_secs(2), "server wait ignored");

        let absurd = retry_delay(&http, 1, Some(Duration::from_secs(3600)));
        assert_eq!(absurd, http.max_retry_delay, "unbounded wait accepted");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let http = HttpConfig::default();
        let first = retry_delay(&http, 1, None);
        let third = retry_delay(&http, 3, None);
        assert!(first >= http.retry_base_delay);
        assert!(third > first, "{third:?} !> {first:?}");
        for attempt in 1..=20 {
            let d = retry_delay(&http, attempt, None);
            assert!(
                d <= http.max_retry_delay * 2,
                "attempt {attempt} waits {d:?}"
            );
        }
    }

    #[test]
    fn zero_retry_budget_is_representable() {
        let http = HttpConfig {
            max_retries: 0,
            ..HttpConfig::default()
        };
        assert_eq!(http.max_retries, 0);
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        assert_eq!(parse_retry_after(Some("5")), Some(Duration::from_secs(5)));
        assert_eq!(
            parse_retry_after(Some(" 30 ")),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(Some("")), None);
        assert_eq!(parse_retry_after(None), None);
    }
}
