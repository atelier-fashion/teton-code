//! Retry policy for the model-download client: exponential backoff with jitter
//! (BR-16).
//!
//! Deliberately **pure**: status classification and delay arithmetic with no
//! HTTP type, no clock, and no sleeping. The one impure corner — the entropy
//! sample that feeds the jitter — is isolated in [`entropy_sample`] so every
//! rule below is unit-testable without a network, a timer, or a random seed.
//!
//! Two ideas carry the policy:
//!
//! - **What is worth retrying.** A `429` (the host is rate-limiting us) and a
//!   `5xx` (the host or its CDN is briefly unavailable) are transient and get a
//!   ladder of retries; every other non-2xx is permanent and fails immediately.
//!   The two transient classes stay *distinct* all the way to the surfaced error
//!   (`RateLimited` vs `Unavailable`), because "slow down" and "come back later"
//!   are different things to tell a user — and both are different from a corrupt
//!   download (AC-12).
//! - **How long to wait.** Doubling from `base_delay`, capped at `max_delay`,
//!   with *equal jitter* (a uniform sample from the upper half of the computed
//!   delay). Jitter matters because every machine that accepts the first-run
//!   consent prompt fetches the same handful of files from the same host: an
//!   unjittered ladder would re-synchronize retries into a thundering herd. A
//!   server-supplied `Retry-After` overrides the ladder (capped at `max_delay`)
//!   and is honoured *as given* — jittering it downward would just earn another
//!   `429`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How a response status reads to the retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// 2xx — the request succeeded; nothing to retry.
    Success,
    /// 429 — the host is rate-limiting us. Retryable (BR-16).
    RateLimited,
    /// 5xx — the host or its CDN is temporarily unavailable. Retryable.
    Unavailable,
    /// Anything else (404, 403, 416, an unfollowed 3xx …) — retrying cannot
    /// help, so the fetch fails immediately rather than burning a ladder.
    Permanent,
}

/// Classify `status` for retry purposes.
#[must_use]
pub fn retry_class(status: u16) -> RetryClass {
    match status {
        200..=299 => RetryClass::Success,
        429 => RetryClass::RateLimited,
        500..=599 => RetryClass::Unavailable,
        _ => RetryClass::Permanent,
    }
}

/// Tuning for the retry ladder.
///
/// The defaults are sized for a multi-gigabyte artifact fetch: a handful of
/// retries spread over a few seconds, which absorbs a CDN hiccup without
/// stalling a user-visible install for minutes. Tests construct their own
/// policy with millisecond delays and `jitter: false` so the ladder is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// How many *retries* follow the first attempt (so `max_retries + 1`
    /// attempts in total before a transient failure is surfaced).
    pub max_retries: u32,
    /// Delay before the first retry; doubles from there.
    pub base_delay: Duration,
    /// Ceiling for any single delay, including a server-supplied `Retry-After`.
    pub max_delay: Duration,
    /// Whether to jitter the computed delay. Always `true` in production; tests
    /// turn it off to assert the exact ladder.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// The delay to wait before retry number `attempt` (0-based), honouring a
    /// server-supplied `retry_after`.
    #[must_use]
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        self.delay_with(attempt, retry_after, entropy_sample())
    }

    /// [`RetryPolicy::delay`] with the jitter entropy supplied by the caller —
    /// the deterministic core, so the policy is testable without a PRNG.
    #[must_use]
    pub fn delay_with(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        entropy: u64,
    ) -> Duration {
        match retry_after {
            // Honoured as given (never jittered down: the host told us how long
            // it wants), but never longer than the policy ceiling.
            Some(after) => after.min(self.max_delay),
            None => {
                let delay = backoff_delay(self.base_delay, self.max_delay, attempt);
                if self.jitter {
                    jittered(delay, entropy)
                } else {
                    delay
                }
            }
        }
    }
}

/// `base * 2^attempt`, capped at `max` and saturating rather than overflowing.
#[must_use]
pub fn backoff_delay(base: Duration, max: Duration, attempt: u32) -> Duration {
    match 2u32.checked_pow(attempt) {
        Some(factor) => base.saturating_mul(factor).min(max),
        // A ladder this long is already pinned to the ceiling.
        None => max,
    }
}

/// Equal jitter: a uniform sample from `[delay / 2, delay]`.
///
/// Full jitter (`[0, delay]`) is the other common choice, but a near-zero delay
/// after a `429` just re-hits the rate limiter; keeping the lower half of the
/// window preserves the backoff while still de-synchronizing clients.
#[must_use]
pub fn jittered(delay: Duration, entropy: u64) -> Duration {
    let total = delay.as_nanos();
    let floor = total / 2;
    let span = total - floor;
    let extra = span * u128::from(entropy) / u128::from(u64::MAX);
    Duration::from_nanos(u64::try_from(floor + extra).unwrap_or(u64::MAX))
}

/// Parse a `Retry-After` header value in its delta-seconds form.
///
/// The HTTP-date form is legal but deliberately unsupported: it would need a
/// date parser and a trusted clock, and returning `None` simply falls back to
/// the backoff ladder — strictly safer than mis-parsing a date into a delay.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A non-repeating 64-bit sample for the jitter.
///
/// Mixes the wall clock with a process-wide counter through one `splitmix64`
/// round. This is a *spreading* function, not a cryptographic one — nothing
/// here is a secret, and pulling in a PRNG dependency for it would be
/// disproportionate.
fn entropy_sample() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    splitmix64(nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// One round of the `splitmix64` mixer.
const fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_classified_into_retryable_and_permanent() {
        let table = [
            (200, RetryClass::Success),
            (206, RetryClass::Success),
            (301, RetryClass::Permanent),
            (403, RetryClass::Permanent),
            (404, RetryClass::Permanent),
            (416, RetryClass::Permanent),
            (429, RetryClass::RateLimited),
            (500, RetryClass::Unavailable),
            (502, RetryClass::Unavailable),
            (503, RetryClass::Unavailable),
            (504, RetryClass::Unavailable),
        ];
        for (status, expected) in table {
            assert_eq!(retry_class(status), expected, "status {status}");
        }
    }

    #[test]
    fn the_backoff_ladder_doubles_and_then_holds_at_the_ceiling() {
        let base = Duration::from_millis(100);
        let max = Duration::from_millis(800);
        let ladder: Vec<Duration> = (0..6).map(|n| backoff_delay(base, max, n)).collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800),
                Duration::from_millis(800),
                Duration::from_millis(800),
            ]
        );
        // A ladder long enough to overflow the doubling still yields the cap,
        // never a panic or a wrapped-around zero delay.
        assert_eq!(backoff_delay(base, max, u32::MAX), max);
    }

    #[test]
    fn jitter_stays_inside_the_upper_half_of_the_delay() {
        let delay = Duration::from_millis(1000);
        for entropy in [0, 1, u64::MAX / 3, u64::MAX / 2, u64::MAX] {
            let jittered = jittered(delay, entropy);
            assert!(
                jittered >= delay / 2 && jittered <= delay,
                "jittered delay {jittered:?} escaped [500ms, 1s] for entropy {entropy}"
            );
        }
        // The extremes are exact, so the window is genuinely spanned.
        assert_eq!(jittered(delay, 0), delay / 2);
        assert_eq!(jittered(delay, u64::MAX), delay);
    }

    #[test]
    fn successive_entropy_samples_differ() {
        // The counter (not just the clock) is what guarantees this, so two
        // samples taken inside the same nanosecond still diverge.
        let samples: Vec<u64> = (0..8).map(|_| entropy_sample()).collect();
        let mut unique = samples.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), samples.len(), "entropy samples repeated");
    }

    #[test]
    fn a_policy_without_jitter_yields_the_exact_ladder() {
        let policy = RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
            jitter: false,
        };
        let ladder: Vec<Duration> = (0..4).map(|n| policy.delay(n, None)).collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Duration::from_millis(40),
            ]
        );
    }

    #[test]
    fn retry_after_overrides_the_ladder_but_is_capped() {
        let policy = RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(8),
            jitter: true,
        };
        // Honoured exactly — not jittered downward into another 429.
        assert_eq!(
            policy.delay(0, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        // …but a host asking for an hour does not stall the install for an hour.
        assert_eq!(
            policy.delay(0, Some(Duration::from_secs(3600))),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn retry_after_parses_delta_seconds_and_rejects_everything_else() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 3 "), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        // The HTTP-date form is unsupported on purpose: fall back to the ladder.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn the_default_policy_is_bounded_and_actually_retries() {
        let policy = RetryPolicy::default();
        assert!(
            policy.max_retries >= 1,
            "a policy that never retries is not one"
        );
        // Worst case the whole ladder is bounded by max_delay per step.
        for attempt in 0..policy.max_retries {
            assert!(policy.delay(attempt, None) <= policy.max_delay);
        }
    }
}
