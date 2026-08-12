//! Presence attestation: the rules, over plain data.
//!
//! This module is **feature-free on purpose** (REQ-570 ADR-B, following the
//! project's "policy is pure, mechanism is gated" pattern — REQ-564,
//! LESSON-499). The mechanism that actually asks a human — `LAContext` on
//! macOS — sits behind a non-default cargo feature CI never compiles. If the
//! binding, single-use and expiry rules lived beside it, the subtlest code in
//! the tree would ship with the least coverage.
//!
//! So every decision lives here, over plain values, and is table-tested with no
//! FFI, no daemon and no socket. [`super::mechanism`] holds only the FFI, and
//! the test double consumes *this* policy rather than a reimplementation — a
//! double with its own copy of the rule tests only that two implementations
//! share each other's bugs.
//!
//! ## The key is the whole question (BR-6, LESSON-495)
//!
//! An attestation is keyed by `(subject, request)` — the connection whose
//! answer it attests, and the one request it authorizes. That is the same shape
//! REQ-569's [`crate::grants::Grant`] uses, and for the same reason: a
//! remembered credential answers every question its key matches, so the key has
//! to encode the whole question. An attestation minted for connection A's
//! consent request cannot answer connection B's, and cannot answer A's *next*
//! one.
//!
//! ## Single-use is a consuming take, not a flag
//!
//! [`AttestationRegistry::consume`] removes the entry. There is deliberately no
//! `used: bool` a caller must remember to set — mirroring REQ-569's
//! `route_of`-read / `resolve`-consume split, where the thing that must happen
//! exactly once is structurally incapable of happening twice.
//!
//! ## Nothing is persisted (BR-6, REQ-569 ADR-C)
//!
//! An attestation dies with the daemon and with its subject connection. It is
//! never cached into a durable credential: that would be a grant with a time
//! window by another name, which is precisely what ADR-C refused to store.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use teton_protocol::RequestId;

use crate::grants::ConnectionId;

use super::AttestationMethod;

/// How long a verified attestation stays usable (BR-6, closes OQ-3).
///
/// Sixty seconds, single-use, **no burst coverage**. The window exists to bound
/// the gap between the human touching the sensor and the grant being minted —
/// not to amortize prompts across a burst.
///
/// OQ-3 asked whether one attestation could cover several requests raised
/// together, and noted the UX cost of "strictly one": with an OS prompt
/// selected, it means a Touch ID prompt on every cross-session resume. The
/// answer is still no. A burst-covering attestation *is* a grant with a time
/// window, and the flooding case it would smooth over is already bounded by
/// [`crate::consent::MAX_PENDING_CONSENTS_PER_CONNECTION`] — so the worst
/// legitimate case is three prompts, and the ordinary case (a user resuming one
/// session) is one.
pub const ATTESTATION_TTL: Duration = Duration::from_secs(60);

/// Why an answer that needed a presence attestation did not get one.
///
/// Every arm mints nothing. They are kept apart all the way out to the wire
/// because BR-7 requires failure, cancellation and timeout to be
/// distinguishable — they have different remedies, and collapsing them into one
/// "denied" would tell a user who cancelled by accident the same thing it tells
/// a user whose hardware is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationRefusal {
    /// An allow-decision arrived carrying no attestation at all (BR-1, BR-3).
    /// This is the REQ-569 self-approval residual's refusal.
    Required,
    /// The human was asked and did not authenticate (`LAError -1`).
    Failed,
    /// The human, or the app, dismissed the prompt (`LAError -2 / -4 / -9`).
    Cancelled,
    /// The window elapsed with no answer.
    TimedOut,
    /// No usable mechanism on this platform (BR-8, BR-11).
    Unavailable(super::UnavailableReason),
    /// An attestation was presented, but not one this registry holds for this
    /// `(subject, request)` — a replay, a different request, or a different
    /// connection (BR-6, AC-5).
    NotBound,
    /// Held, but past [`ATTESTATION_TTL`] (BR-6, AC-5).
    Expired,
}

impl AttestationRefusal {
    /// The stable wire code for this refusal.
    ///
    /// Distinct strings rather than one code with a message, because AC-6
    /// asserts the *taxonomy*: a test that keyed on prose would pass while the
    /// three endings BR-7 separates silently collapsed into one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Required => "ATTESTATION_REQUIRED",
            Self::Failed => "ATTESTATION_FAILED",
            Self::Cancelled => "ATTESTATION_CANCELLED",
            Self::TimedOut => "ATTESTATION_TIMEOUT",
            Self::Unavailable(_) => "ATTESTATION_UNAVAILABLE",
            Self::NotBound => "ATTESTATION_NOT_BOUND",
            Self::Expired => "ATTESTATION_EXPIRED",
        }
    }
}

/// A verified presence check, bound to one connection and one request.
///
/// Constructed only by [`PresenceAttestation::verified`], which refuses
/// [`AttestationMethod::None`] — so "no attestation" can never be represented as
/// an attestation whose method happens to be none. That is the Permissions
/// table's "mint a grant with `attested_by.method == none` → nothing", enforced
/// by the type rather than by every caller remembering to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceAttestation {
    method: AttestationMethod,
    verified_at: Instant,
    subject: ConnectionId,
    request: RequestId,
}

impl PresenceAttestation {
    /// Record a mechanism's successful verification.
    ///
    /// `None` returns `None`: there is no such thing as an attestation that
    /// attests nothing, and letting one exist would make the security-critical
    /// question ("is this a real attestation?") a field comparison somebody has
    /// to remember instead of a construction that cannot happen.
    #[must_use]
    pub fn verified(
        method: AttestationMethod,
        subject: ConnectionId,
        request: RequestId,
        verified_at: Instant,
    ) -> Option<Self> {
        if method == AttestationMethod::None {
            return None;
        }
        Some(Self {
            method,
            verified_at,
            subject,
            request,
        })
    }

    /// The mechanism that verified this presence — reported on `grant_minted`
    /// (BR-9, AC-9).
    #[must_use]
    pub fn method(&self) -> AttestationMethod {
        self.method
    }

    /// The connection this attestation speaks for. Never transferable.
    #[must_use]
    pub fn subject(&self) -> ConnectionId {
        self.subject
    }

    /// The one request it authorizes.
    #[must_use]
    pub fn request(&self) -> &RequestId {
        &self.request
    }

    /// Whether this attestation is still inside its window at `now`.
    #[must_use]
    pub fn is_live_at(&self, now: Instant) -> bool {
        now.duration_since(self.verified_at) < ATTESTATION_TTL
    }
}

/// The daemon's live attestations.
///
/// Shaped after [`crate::grants::GrantRegistry`] deliberately: a locked map
/// whose *whole key* is the question, and a `release` so nothing outlives the
/// connection it was minted for.
#[derive(Debug, Default)]
pub struct AttestationRegistry {
    /// `(subject, request)` → the attestation. Both halves are in the key
    /// (BR-6): neither alone is the question being asked.
    live: Mutex<HashMap<(ConnectionId, RequestId), PresenceAttestation>>,
}

impl AttestationRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified attestation, ready to be consumed once.
    ///
    /// Replaces any attestation already held for the same `(subject, request)`.
    /// That is safe where overwriting a *consent route* would not be: a route
    /// decides who may answer, so rewriting it steals a live prompt, whereas an
    /// attestation is the answerer's own freshly-verified presence for a
    /// question that is already theirs. Re-verifying replaces a stale proof of
    /// the same fact.
    pub fn record(&self, attestation: PresenceAttestation) {
        self.live
            .lock()
            .expect("attestation registry lock poisoned")
            .insert(
                (attestation.subject(), attestation.request().clone()),
                attestation,
            );
    }

    /// Take the attestation authorizing `subject` to answer `request`, or say
    /// why not.
    ///
    /// **Consuming** — this is what makes single-use structural (BR-6, AC-5). A
    /// second call with the same key finds nothing and refuses
    /// [`AttestationRefusal::NotBound`], which is also the answer a replay from
    /// a *different* connection or for a *different* request gets: all three are
    /// the same failure, "no attestation is held for this exact question".
    ///
    /// An expired entry is removed as it is refused, so a stale proof cannot sit
    /// in the map waiting for a clock to be wrong (BR-7's "leaves no partial
    /// state").
    pub fn consume(
        &self,
        subject: ConnectionId,
        request: &RequestId,
        now: Instant,
    ) -> Result<PresenceAttestation, AttestationRefusal> {
        let mut live = self
            .live
            .lock()
            .expect("attestation registry lock poisoned");
        match live.remove(&(subject, request.clone())) {
            None => Err(AttestationRefusal::NotBound),
            Some(attestation) if attestation.is_live_at(now) => Ok(attestation),
            // Removed above, so the expired entry is gone either way.
            Some(_) => Err(AttestationRefusal::Expired),
        }
    }

    /// Drop every attestation held for `connection` — called when it ends.
    ///
    /// Unconditional, for the reason [`crate::grants::GrantRegistry::release`]
    /// is: the rule is "attestations die with their subject", and a release that
    /// only ran where the caller believed one existed would be a rule enforced
    /// by the caller's bookkeeping instead of by the registry's.
    pub fn release(&self, connection: ConnectionId) {
        self.live
            .lock()
            .expect("attestation registry lock poisoned")
            .retain(|(subject, _), _| *subject != connection);
    }

    /// How many attestations are live.
    ///
    /// Exists so a test can assert an **absence** — AC-6 requires the registries
    /// to be inspected rather than the outcome inferred from an error code.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
            .lock()
            .expect("attestation registry lock poisoned")
            .len()
    }

    /// Whether any attestation is live at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::grants::GrantRegistry;

    fn request(name: &str) -> RequestId {
        RequestId::from(name)
    }

    /// `None` is a recorded value, never a constructible attestation.
    #[test]
    fn an_attestation_can_never_be_built_from_the_none_method() {
        let registry = GrantRegistry::new();
        let subject = registry.next_connection_id();
        assert!(PresenceAttestation::verified(
            AttestationMethod::None,
            subject,
            request("consent-0"),
            Instant::now()
        )
        .is_none());
        assert!(PresenceAttestation::verified(
            AttestationMethod::OsBiometric,
            subject,
            request("consent-0"),
            Instant::now()
        )
        .is_some());
    }

    /// **AC-5 / BR-6.** The whole key is the question: all four cells of
    /// (right/wrong subject) x (right/wrong request).
    ///
    /// The cross cells are the point. An attestation that answered a *different*
    /// request would be a burst-covering credential OQ-3 explicitly refused, and
    /// one that answered for a *different* connection would be transferable —
    /// the two properties BR-6 names as making it "exactly one decision".
    #[test]
    fn an_attestation_answers_only_its_own_subject_and_its_own_request() {
        let grants = GrantRegistry::new();
        let mine = grants.next_connection_id();
        let theirs = grants.next_connection_id();
        let now = Instant::now();

        let cases = [
            ("the exact pair it was minted for", mine, "consent-0", true),
            (
                "a different request, same connection",
                mine,
                "consent-1",
                false,
            ),
            (
                "the same request, a different connection",
                theirs,
                "consent-0",
                false,
            ),
            ("neither half matching", theirs, "consent-1", false),
        ];

        for (case, subject, req, expected) in cases {
            let registry = AttestationRegistry::new();
            registry.record(
                PresenceAttestation::verified(
                    AttestationMethod::OsBiometric,
                    mine,
                    request("consent-0"),
                    now,
                )
                .expect("a biometric attestation is constructible"),
            );

            let got = registry.consume(subject, &request(req), now);
            assert_eq!(got.is_ok(), expected, "{case}");
            if !expected {
                assert_eq!(got.unwrap_err(), AttestationRefusal::NotBound, "{case}");
                assert_eq!(
                    registry.len(),
                    1,
                    "{case}: a refused consume must not consume somebody else's attestation"
                );
            }
        }
    }

    /// **AC-5.** Single-use: replaying the very same attestation is refused.
    #[test]
    fn an_attestation_is_consumed_exactly_once() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let registry = AttestationRegistry::new();
        let now = Instant::now();
        let id = request("consent-0");

        registry.record(
            PresenceAttestation::verified(AttestationMethod::OsBiometric, subject, id.clone(), now)
                .expect("constructible"),
        );

        assert!(registry.consume(subject, &id, now).is_ok());
        assert!(
            registry.is_empty(),
            "a consumed attestation must leave nothing behind"
        );
        assert_eq!(
            registry.consume(subject, &id, now).unwrap_err(),
            AttestationRefusal::NotBound,
            "a replay must be refused"
        );
    }

    /// **AC-5.** Expiry, and the expired entry is dropped as it is refused.
    #[test]
    fn an_attestation_expires_and_leaves_nothing_behind() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let registry = AttestationRegistry::new();
        let minted = Instant::now();
        let id = request("consent-0");

        registry.record(
            PresenceAttestation::verified(
                AttestationMethod::OsCredential,
                subject,
                id.clone(),
                minted,
            )
            .expect("constructible"),
        );

        // One tick inside the window is still good.
        let inside = minted + ATTESTATION_TTL - Duration::from_millis(1);
        assert!(registry.consume(subject, &id, inside).is_ok());

        // And one tick past it is not.
        registry.record(
            PresenceAttestation::verified(
                AttestationMethod::OsCredential,
                subject,
                id.clone(),
                minted,
            )
            .expect("constructible"),
        );
        let outside = minted + ATTESTATION_TTL;
        assert_eq!(
            registry.consume(subject, &id, outside).unwrap_err(),
            AttestationRefusal::Expired
        );
        assert!(
            registry.is_empty(),
            "an expired attestation must not sit in the map waiting for a wrong clock"
        );
    }

    /// Attestations die with their subject, and only with *that* subject.
    #[test]
    fn releasing_a_connection_drops_its_attestations_and_no_others() {
        let grants = GrantRegistry::new();
        let leaving = grants.next_connection_id();
        let staying = grants.next_connection_id();
        let registry = AttestationRegistry::new();
        let now = Instant::now();

        for (subject, req) in [
            (leaving, "consent-0"),
            (leaving, "consent-1"),
            (staying, "consent-2"),
        ] {
            registry.record(
                PresenceAttestation::verified(
                    AttestationMethod::OsBiometric,
                    subject,
                    request(req),
                    now,
                )
                .expect("constructible"),
            );
        }
        assert_eq!(registry.len(), 3);

        registry.release(leaving);
        assert_eq!(
            registry.len(),
            1,
            "only the departed connection's are dropped"
        );
        assert!(registry
            .consume(staying, &request("consent-2"), now)
            .is_ok());
    }

    /// **AC-6 / BR-7.** The refusal taxonomy is distinct all the way to the wire.
    ///
    /// Asserted as a set rather than arm by arm: what would actually go wrong is
    /// two arms quietly sharing a code, and only comparing them all catches that.
    #[test]
    fn every_refusal_has_its_own_stable_code() {
        use std::collections::HashSet;
        let all = [
            AttestationRefusal::Required,
            AttestationRefusal::Failed,
            AttestationRefusal::Cancelled,
            AttestationRefusal::TimedOut,
            AttestationRefusal::Unavailable(super::super::UnavailableReason::NoPolkitAgent),
            AttestationRefusal::NotBound,
            AttestationRefusal::Expired,
        ];
        let codes: HashSet<&str> = all.iter().map(AttestationRefusal::code).collect();
        assert_eq!(
            codes.len(),
            all.len(),
            "two refusals share a wire code, so BR-7's endings are not distinguishable"
        );
    }
}
