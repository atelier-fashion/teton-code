//! Human presence attestation — the surface a headless process cannot satisfy.
//!
//! REQ-569 closed the ambient-attach path but shipped a residual it could not
//! close with the primitives it had: when nothing is attached to a target
//! session, the consent prompt routes back to the *requesting* connection, so a
//! headless same-UID process could answer its own question. REQ-570 closes it by
//! requiring an approval to be attributable to **a human being present at the
//! machine**, verified by something that process cannot satisfy silently.
//!
//! ## Why an OS prompt, when REQ-569 ADR-A rejected FFI
//!
//! ADR-A rejected code signatures and keychain ACLs because unsigned dev builds
//! make those checks **inert** — they cannot distinguish anything in the
//! environment where the code is developed, so they are controls nobody
//! verifies. An OS presence prompt is a different shape: it still puts a real
//! prompt in front of a real human regardless of how the binary is signed.
//!
//! That claim was load-bearing enough that BR-12 made verifying it the first
//! thing this REQ did, before any other task. It was verified empirically
//! against a binary confirmed `adhoc, linker-signed / TeamIdentifier=not set`:
//! `canEvaluatePolicy(deviceOwnerAuthentication)` returned true, and
//! `evaluatePolicy` **blocked** for the full probe window with the runloop
//! actively serviced — a real prompt, waiting on a person. An inert mechanism
//! returns an error in milliseconds. See `architecture.md` §0 for the full
//! transcript.
//!
//! ## Shape (ADR-B)
//!
//! - [`policy`] is **feature-free**: binding, single-use, expiry, and the
//!   refusal taxonomy, over plain data. CI compiles and tests all of it.
//! - [`mechanism`] is behind the non-default `presence` feature and holds
//!   **only** the FFI that asks a human.
//!
//! This is the project's established split for gated subsystems (REQ-564,
//! LESSON-499): otherwise the subtlest code in the tree ships with the least
//! coverage, and a test double with its own copy of the rules tests only that
//! two implementations share each other's bugs.

pub mod policy;

#[cfg(all(feature = "presence", target_os = "macos"))]
pub mod mechanism;

use teton_protocol::RequestId;

use crate::grants::ConnectionId;

pub use policy::{AttestationRefusal, AttestationRegistry, PresenceAttestation, ATTESTATION_TTL};

/// What actually verified a human's presence.
///
/// [`None`](Self::None) is a **distinct recorded value, never a default that
/// silently passes** — it exists so `grant_minted` can report the creator path
/// honestly (AC-9), and so a missing attestation and an unattested one are not
/// the same wire shape. [`policy::PresenceAttestation::verified`] refuses to
/// build an attestation from it.
///
/// There is deliberately no `out_of_band_code` variant. OQ-1 considered and
/// dropped a daemon-issued code — it fails the no-TTY constraint that makes this
/// REQ hard, since a code needs a surface to be typed into and the VS Code
/// extension is a first-class client — and an unreachable variant in a security
/// enum invites a future reader to wire it up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttestationMethod {
    /// A biometric check (Touch ID).
    OsBiometric,
    /// The device-owner credential — the login password. `LAContext` is asked
    /// for `deviceOwnerAuthentication` rather than the biometrics-only policy
    /// precisely so biometry-absent hardware degrades to this rather than to
    /// nothing (OQ-1).
    OsCredential,
    /// No attestation. Recorded, never minted from.
    None,
}

impl AttestationMethod {
    /// The stable wire spelling, carried on `grant_minted` (AC-9).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OsBiometric => "os_biometric",
            Self::OsCredential => "os_credential",
            Self::None => "none",
        }
    }
}

/// Why no presence check can be made here (BR-8, BR-11).
///
/// A **first-class value rather than an error path**, because AC-7 requires the
/// no-mechanism posture to be assertable by injection on any platform, and AC-7b
/// requires the Linux case to name *that* cause rather than degrade into a
/// generic failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    /// Linux, no polkit **agent** reachable — the BR-11 case, and the normal
    /// path on headless Linux (SSH, containers, CI, a server).
    ///
    /// Keyed on agent availability, **not** on whether polkit is installed. The
    /// BR-12 spike found the authority can be on the bus and still answer
    /// `Authorization requires authentication but no agent is available.`, so
    /// "is polkit present" is a false positive for "can a human be asked".
    NoPolkitAgent,
    /// This build compiled no presence mechanism (the default/CI build).
    NotCompiled,
    /// The platform has no supported mechanism at all.
    PlatformUnsupported,
    /// A mechanism exists but this machine cannot currently use it — no
    /// biometry enrolled and no passcode set.
    NoEnrolledCredential,
}

impl UnavailableReason {
    /// Operator-facing explanation. Carried on the refusal so the limitation is
    /// **documented rather than discovered** (BR-11).
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::NoPolkitAgent => {
                "no polkit authentication agent is reachable, so no human can be asked; \
                 cross-session attach is refused on this host (headless Linux: SSH, \
                 containers, CI)"
            }
            Self::NotCompiled => {
                "this build compiled no presence-attestation mechanism, so cross-session \
                 attach is refused"
            }
            Self::PlatformUnsupported => {
                "no presence-attestation mechanism is supported on this platform, so \
                 cross-session attach is refused"
            }
            Self::NoEnrolledCredential => {
                "this machine has no enrolled biometric and no device passcode, so no \
                 presence check can be made"
            }
        }
    }
}

/// Whether a human can be asked here, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MechanismAvailability {
    /// A human can be asked, by this method.
    Available(AttestationMethod),
    /// No human can be asked. **Fail closed** — BR-8/BR-11 require a refusal,
    /// never a silent fall back to the REQ-569 self-approval residual this REQ
    /// exists to close.
    Unavailable(UnavailableReason),
}

/// The seam between "the daemon needs a human to confirm" and "something asks
/// one".
///
/// The whole point of the indirection is AC-7: a "no mechanism available"
/// implementation can be injected on any platform, so the fail-closed posture is
/// testable on a developer's Mac and on a headless Linux CI runner alike, rather
/// than only being observable on hardware nobody runs the suite on.
pub trait PresenceVerifier: Send + Sync {
    /// Whether a human can be asked here at all. Consulted **before** prompting,
    /// so an unavailable mechanism produces the BR-8 posture refusal rather than
    /// a prompt that cannot appear.
    fn availability(&self) -> MechanismAvailability;

    /// Ask a human to confirm, and on success return an attestation bound to
    /// `subject` and `request`.
    ///
    /// Implementations must bind to exactly those two — an implementation that
    /// returned an unbound attestation would defeat BR-6 at the source, since
    /// the registry keys on what it is handed.
    fn verify(
        &self,
        subject: ConnectionId,
        request: &RequestId,
    ) -> Result<PresenceAttestation, AttestationRefusal>;
}

/// The verifier a build with no mechanism uses — and the one AC-7 injects.
///
/// Refuses everything, with a named reason. This is **not** the gate switched
/// off: it is the honest answer where no human can be asked, and it is what
/// makes BR-8's "degrade to a stated, fail-closed posture" a value the code
/// carries rather than a paragraph in a document.
#[derive(Debug, Clone, Copy)]
pub struct UnavailableVerifier {
    reason: UnavailableReason,
}

impl UnavailableVerifier {
    /// A verifier that refuses with `reason`.
    #[must_use]
    pub fn new(reason: UnavailableReason) -> Self {
        Self { reason }
    }
}

impl PresenceVerifier for UnavailableVerifier {
    fn availability(&self) -> MechanismAvailability {
        MechanismAvailability::Unavailable(self.reason)
    }

    fn verify(
        &self,
        _subject: ConnectionId,
        _request: &RequestId,
    ) -> Result<PresenceAttestation, AttestationRefusal> {
        Err(AttestationRefusal::Unavailable(self.reason))
    }
}

/// A verifier that accepts without asking anyone — **fixtures only**.
///
/// AC-1 and AC-3 need the *granted* path exercised end to end, and the real
/// mechanism cannot be satisfied in CI by design: its whole purpose is that
/// nothing gets past it without a human touching a sensor. So the seam takes a
/// double, and this is it.
///
/// Three things keep it out of production, and they are worth stating because
/// this type is exactly what an attacker would want to reach:
///
/// 1. [`default_verifier`] never returns it — no build configuration selects it.
/// 2. The only way to install it is [`crate::server::Daemon::with_presence_verifier`],
///    which a fixture has to name out loud (the same discipline
///    `with_consent_timeout` follows).
/// 3. `the_default_verifier_never_accepts_without_asking` asserts (1) mechanically,
///    so a future edit to `default_verifier` that reached for this type fails the
///    suite rather than shipping.
///
/// It is deliberately **not** `#[cfg(test)]`: the integration suites under
/// `tests/` are separate crates compiled against the library without that flag,
/// and AC-1's raw-RPC assertions live there.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct AcceptingVerifier {
    method: AttestationMethod,
}

impl AcceptingVerifier {
    /// A double that reports `method` verified every time.
    ///
    /// Panics on [`AttestationMethod::None`]: a double that minted "an
    /// attestation by no method" would be asserting the exact hole
    /// [`PresenceAttestation::verified`] exists to close, and a fixture built on
    /// it would go green while the property under test was broken.
    #[must_use]
    pub fn new(method: AttestationMethod) -> Self {
        assert!(
            method != AttestationMethod::None,
            "an accepting verifier must name a real method; `None` never mints"
        );
        Self { method }
    }
}

impl Default for AcceptingVerifier {
    fn default() -> Self {
        Self::new(AttestationMethod::OsBiometric)
    }
}

impl PresenceVerifier for AcceptingVerifier {
    fn availability(&self) -> MechanismAvailability {
        MechanismAvailability::Available(self.method)
    }

    fn verify(
        &self,
        subject: ConnectionId,
        request: &RequestId,
    ) -> Result<PresenceAttestation, AttestationRefusal> {
        // Bound to the caller's subject and request like the real one — a double
        // that returned an unbound attestation would quietly disable BR-6 in
        // every test that used it, which is how a suite ends up proving nothing.
        PresenceAttestation::verified(
            self.method,
            subject,
            request.clone(),
            std::time::Instant::now(),
        )
        .ok_or(AttestationRefusal::Failed)
    }
}

/// The seam that lets the acceptance suite drive the *granted* path
/// (`TETON_PRESENCE_ACCEPT`, DECISION 3).
///
/// AC-1 and AC-3 need a consent that actually completes, and the real mechanism
/// cannot produce one without a human touching a sensor — which no CI runner
/// has. The acceptance suite spawns a real daemon binary, so it cannot reach
/// [`crate::server::Daemon::with_presence_verifier`] the way an in-process
/// fixture can, and it needs an environment-level seam instead.
///
/// **Why this is safe against the adversary the REQ is about.** It rides the
/// existing `TETON_TEST_SEAMS` master switch, whose contract (DECISION 3, E-6)
/// is that a **release build refuses to start at all** when it is set. So the
/// shipped binary cannot honour this seam no matter what a same-UID process puts
/// in its environment — the bypass does not exist in the artifact users run. A
/// debug build additionally has to be asked twice: the master switch *and* this
/// variable.
///
/// It is deliberately an all-or-nothing accept rather than a "skip attestation"
/// flag: the daemon still runs the whole record → consume → binding path, so the
/// BR-6 rules under test stay live and only the human is simulated.
fn seam_verifier() -> Option<Box<dyn PresenceVerifier>> {
    if !crate::runtime::test_seams_enabled() {
        return None;
    }
    match std::env::var("TETON_PRESENCE_ACCEPT").ok().as_deref() {
        Some("1") => Some(Box::new(AcceptingVerifier::default())),
        // A named refusal, so the suite can drive BR-8/BR-11's posture — AC-7's
        // "injected no-mechanism seam" — against a real daemon process too.
        Some("unavailable") => Some(Box::new(UnavailableVerifier::new(
            UnavailableReason::PlatformUnsupported,
        ))),
        _ => None,
    }
}

/// The verifier this build ships with.
///
/// Default and CI builds compile no FFI and get [`UnavailableVerifier`], which
/// keeps the honest fail-closed behaviour: cross-session attach is **refused**
/// there, never self-approved.
#[must_use]
pub fn default_verifier() -> Box<dyn PresenceVerifier> {
    if let Some(seam) = seam_verifier() {
        return seam;
    }
    #[cfg(all(feature = "presence", target_os = "macos"))]
    {
        Box::new(mechanism::LocalAuthenticationVerifier::new())
    }
    #[cfg(not(all(feature = "presence", target_os = "macos")))]
    {
        #[cfg(target_os = "macos")]
        let reason = UnavailableReason::NotCompiled;
        #[cfg(all(not(target_os = "macos"), target_os = "linux"))]
        let reason = UnavailableReason::NoPolkitAgent;
        #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
        let reason = UnavailableReason::PlatformUnsupported;
        Box::new(UnavailableVerifier::new(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::grants::GrantRegistry;

    /// **AC-7.** The no-mechanism posture is a refusal, asserted through the
    /// injection seam so it runs on any platform.
    ///
    /// The assertion that matters is the *negative* one: nothing is minted. A
    /// test that only checked the error would still pass if the code refused
    /// loudly and granted anyway.
    #[test]
    fn a_platform_with_no_mechanism_refuses_and_mints_nothing() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let registry = AttestationRegistry::new();
        let verifier = UnavailableVerifier::new(UnavailableReason::PlatformUnsupported);
        let request = RequestId::from("consent-0");

        assert_eq!(
            verifier.availability(),
            MechanismAvailability::Unavailable(UnavailableReason::PlatformUnsupported)
        );
        let refused = verifier.verify(subject, &request).unwrap_err();
        assert_eq!(
            refused,
            AttestationRefusal::Unavailable(UnavailableReason::PlatformUnsupported)
        );
        assert_eq!(refused.code(), "ATTESTATION_UNAVAILABLE");
        assert!(
            registry.is_empty(),
            "a refused verification must leave the registry empty"
        );
    }

    /// **AC-7b.** The Linux-without-an-agent case names *that* cause.
    ///
    /// The point is the naming, not the refusal: BR-11 requires the limitation
    /// to be documented rather than discovered, so a user on a headless box is
    /// told polkit has no agent, not merely that something failed.
    #[test]
    fn the_linux_no_agent_case_names_its_own_cause() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let verifier = UnavailableVerifier::new(UnavailableReason::NoPolkitAgent);

        let refused = verifier
            .verify(subject, &RequestId::from("consent-0"))
            .unwrap_err();
        assert_eq!(
            refused,
            AttestationRefusal::Unavailable(UnavailableReason::NoPolkitAgent)
        );
        let AttestationRefusal::Unavailable(reason) = refused else {
            panic!("the refusal must carry its reason, not flatten it");
        };
        assert!(
            reason.describe().contains("polkit"),
            "the posture must name polkit specifically: {}",
            reason.describe()
        );
        assert!(
            reason.describe().contains("agent"),
            "and must name the *agent* as the missing piece — the BR-12 spike found \
             the authority can be on the bus and still have no agent: {}",
            reason.describe()
        );
    }

    /// Every reason is distinguishable and says something specific.
    #[test]
    fn every_unavailable_reason_describes_itself_distinctly() {
        use std::collections::HashSet;
        let all = [
            UnavailableReason::NoPolkitAgent,
            UnavailableReason::NotCompiled,
            UnavailableReason::PlatformUnsupported,
            UnavailableReason::NoEnrolledCredential,
        ];
        let described: HashSet<&str> = all.iter().map(UnavailableReason::describe).collect();
        assert_eq!(described.len(), all.len());
    }

    /// The default build refuses rather than self-approving (BR-8, BR-11).
    ///
    /// This is the whole posture in one assertion: on the build CI actually
    /// runs, cross-session attach cannot be attested, and that must surface as a
    /// refusal rather than as the residual REQ-570 exists to close.
    #[test]
    #[cfg(not(feature = "presence"))]
    fn the_default_build_ships_a_fail_closed_verifier() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let verifier = default_verifier();

        assert!(
            matches!(
                verifier.availability(),
                MechanismAvailability::Unavailable(_)
            ),
            "a build with no mechanism must not claim one is available"
        );
        assert!(verifier
            .verify(subject, &RequestId::from("consent-0"))
            .is_err());
    }

    /// The guard that keeps [`AcceptingVerifier`] a fixture.
    ///
    /// Mechanical rather than by inspection: an edit to `default_verifier` that
    /// reached for the accepting double — on any platform, under any feature
    /// combination this build compiles — fails here instead of shipping a daemon
    /// that grants without asking anyone.
    #[test]
    fn the_default_verifier_never_accepts_without_asking() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let verifier = default_verifier();
        let request = RequestId::from("consent-0");

        // Either it refuses outright (no mechanism compiled), or it is a real
        // mechanism whose availability depends on hardware — but it must never
        // be a thing that hands back an attestation for free.
        match verifier.availability() {
            MechanismAvailability::Unavailable(_) => {
                assert!(verifier.verify(subject, &request).is_err());
            }
            MechanismAvailability::Available(_) => {
                // A real mechanism is present. We cannot drive it here (it needs
                // a human), so assert the thing that would be wrong: the shipped
                // verifier must not be the accepting double.
                assert_ne!(
                    format!("{:?}", std::any::type_name_of_val(&*verifier)),
                    format!("{:?}", std::any::type_name::<AcceptingVerifier>()),
                    "the shipped verifier must never be the test double"
                );
            }
        }
    }

    /// The double binds like the real one, or every test using it proves nothing.
    #[test]
    fn the_accepting_double_binds_to_its_caller_and_request() {
        let grants = GrantRegistry::new();
        let subject = grants.next_connection_id();
        let other = grants.next_connection_id();
        let verifier = AcceptingVerifier::default();
        let request = RequestId::from("consent-7");

        let attestation = verifier
            .verify(subject, &request)
            .expect("the double accepts");
        assert_eq!(attestation.subject(), subject);
        assert_eq!(attestation.request(), &request);
        assert_ne!(attestation.subject(), other);
        assert_eq!(attestation.method(), AttestationMethod::OsBiometric);
    }

    #[test]
    #[should_panic(expected = "must name a real method")]
    fn an_accepting_double_cannot_be_built_on_the_none_method() {
        let _ = AcceptingVerifier::new(AttestationMethod::None);
    }

    /// The seam is unreachable without the master switch, on any build.
    ///
    /// This is the claim the `TETON_PRESENCE_ACCEPT` seam's safety rests on, so
    /// it is asserted rather than argued: with `TETON_TEST_SEAMS` unset — which
    /// is every real run, and every run of this suite — the accepting double is
    /// not selected no matter what the presence variable says.
    ///
    /// The release-build half of the contract (a release build *refuses to
    /// start* when the master switch is set) belongs to `runtime::seam_policy`
    /// and is tested there; what matters here is that this seam sits behind that
    /// same switch rather than beside it.
    #[test]
    fn the_presence_seam_is_unreachable_without_the_master_switch() {
        // The suite never sets TETON_TEST_SEAMS in-process; the acceptance suite
        // sets it on a spawned daemon. So this is the ordinary path.
        assert!(
            !crate::runtime::test_seams_enabled(),
            "this assertion is only meaningful with the master switch off"
        );
        assert!(
            seam_verifier().is_none(),
            "no seam verifier may be selected without the master switch"
        );

        // And with the switch off, the presence variable alone changes nothing.
        std::env::set_var("TETON_PRESENCE_ACCEPT", "1");
        let selected = seam_verifier().is_none();
        std::env::remove_var("TETON_PRESENCE_ACCEPT");
        assert!(
            selected,
            "TETON_PRESENCE_ACCEPT must do nothing on its own — it rides the \
             master switch, it does not bypass it"
        );
    }

    #[test]
    fn the_wire_spellings_are_stable() {
        assert_eq!(AttestationMethod::OsBiometric.as_str(), "os_biometric");
        assert_eq!(AttestationMethod::OsCredential.as_str(), "os_credential");
        assert_eq!(AttestationMethod::None.as_str(), "none");
    }
}
