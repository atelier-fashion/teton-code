//! The egress enforcement decision (BR-1), as a pure function.
//!
//! Given a request's [`Provenance`] and the configured privacy boundaries, decide
//! whether the request may leave the machine. This module makes **no** I/O and
//! emits **no** events — it only answers "does this provenance intersect a
//! boundary that forbids egress, and if so which source is the offender?". The
//! [`crate::egress::Egress`] choke point turns a [`Inspection::Blocked`] into a
//! `privacy_block` event and a typed error.
//!
//! Keeping the decision pure means it is exhaustively unit-testable in isolation
//! (mirroring `teton_core::boundary`, which it composes) and that the same logic
//! backs both the guarded `send` path and the adapter-facing transport.
//!
//! ## Fail-closed on every boundary mode
//!
//! [`BoundaryMode::LocalOnly`] content is blocked — the hard BR-1 guarantee.
//! [`BoundaryMode::RedactThenRemote`] is a post-MVP mode (OQ-7) whose redactor
//! does not exist yet; until it does, egress **also** blocks that content rather
//! than leak it un-redacted. So any boundary-tagged source is held local. This is
//! deliberately stricter than the eventual behavior and is documented as such.
//!
//! **REQ-562 did not change this.** The `redact` that now exists *detects and
//! blocks* — BR-5 says in as many words that v1 never rewrites a payload. What
//! `RedactThenRemote` needs is a **substituting** redactor, one that replaces
//! the sensitive spans and sends the rest, and that is a separate REQ. So the
//! mode still has no implementation and this arm still fails closed.

use teton_core::boundary::BoundaryMatcher;
use teton_protocol::events::{PrivacyAction, ProvenanceRejection};

use crate::egress::provenance::{
    sanitize_reported_source, Provenance, MALFORMED_PROVENANCE_PATH, UNKNOWN_PROVENANCE_PATH,
};

/// The outcome of inspecting a request's provenance against the boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    /// No source in the provenance is under a boundary; the request may proceed.
    Allowed,
    /// A source is governed by a boundary that forbids egress.
    Blocked(Violation),
}

impl Inspection {
    /// Whether the request was blocked.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Inspection::Blocked(_))
    }

    /// The violation, if this inspection blocked the request.
    #[must_use]
    pub fn violation(&self) -> Option<&Violation> {
        match self {
            Inspection::Blocked(v) => Some(v),
            Inspection::Allowed => None,
        }
    }
}

/// A boundary violation: the offending source and the action egress will take.
///
/// Carries only the *path* (config-authored, already known to be under a
/// boundary) and the action — never any file content. This is what makes the
/// error/telemetry paths safe (BR-1): a `Violation` can be logged or serialized
/// without leaking a single byte of boundary content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Repo-relative path of the boundary source that would have leaked.
    pub path: String,
    /// The action the egress choke point takes on this violation.
    pub action: PrivacyAction,
}

/// A provenance source whose string form is not the canonical identity boundary
/// matching is defined against (REQ-571 ADR-D).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedSource {
    /// The offending source, already sanitized for reporting.
    pub source: String,
    /// Which canonical-form rule it broke.
    pub reason: ProvenanceRejection,
}

/// The first (lowest, deterministic) source in `provenance` that is not in
/// canonical form, if any — ADR-D's fail-closed guard.
///
/// Canonical form is: repo-root-relative, `/`-separated, no `.`, `..`, or empty
/// segment. A source that breaks it would not *fail* boundary matching, it would
/// silently **pass** it — `secrets/prod.env` is covered by `secrets/**` and
/// `/repo/secrets/prod.env` is covered by nothing — so a malformed source is the
/// one shape where matching-and-allowing is the wrong answer. Hence: refused,
/// ahead of matching, whether or not any boundary is configured.
///
/// # This is unreachable, and that is why it is here (LESSON-508)
///
/// After ADR-A a [`Provenance`] holds
/// [`ProvenanceId`](teton_core::ProvenanceId)s, and both of that type's
/// constructors run one validator that refuses every spelling this function
/// looks for. So no typed path in the daemon can hand this guard a value it
/// rejects, and the honest description of the code below is *dead*.
///
/// It stays because "unreachable" is a property of today's call graph, not of
/// the type: `ProvenanceId::claimed` exists precisely to accept a **third
/// party's** assertion, and the next constructor — or the next crate to hold a
/// provenance — inherits none of today's guarantees. LESSON-508 is the rule that
/// a redundant guard without its own test is one refactor from being deleted as
/// noise, so this one is tested through
/// [`ProvenanceId::unvalidated_for_test`](teton_core::ProvenanceId::unvalidated_for_test),
/// a seam that exists only in test builds. Deleting the guard turns those tests
/// red rather than turning nothing red.
#[must_use]
pub fn first_malformed_source(provenance: &Provenance) -> Option<MalformedSource> {
    provenance.sources().find_map(|source| {
        malformed_reason(source).map(|reason| MalformedSource {
            source: sanitize_reported_source(source),
            reason,
        })
    })
}

/// Why `source` is not in canonical form, or `None` if it is.
///
/// The checks run in the order a reader would ask them, and each is the same
/// rule `teton_core`'s mint applies — stated here in terms of the *already
/// minted* string rather than the raw input, because that is what this guard
/// receives. Separator normalization is deliberately not repeated: an id that
/// still carries a `\` never went through the mint, and `\`-spelled absolute and
/// traversal forms are caught by the tests below.
fn malformed_reason(source: &str) -> Option<ProvenanceRejection> {
    let normalized = source.replace('\\', "/");
    if normalized.is_empty() {
        return Some(ProvenanceRejection::Empty);
    }
    if normalized.starts_with('/') || std::path::Path::new(source).is_absolute() {
        return Some(ProvenanceRejection::Absolute);
    }
    let mut segments = 0_usize;
    for segment in normalized.split('/') {
        match segment {
            ".." => return Some(ProvenanceRejection::ParentTraversal),
            "." | "" => return Some(ProvenanceRejection::NotCanonical),
            _ => segments += 1,
        }
    }
    (segments == 0).then_some(ProvenanceRejection::Empty)
}

/// Inspect `provenance` against `matcher`, taking `action` when a boundary is
/// hit.
///
/// Returns [`Inspection::Blocked`] for the first (lowest, deterministic) source
/// path that any boundary covers. Sources are iterated in sorted order so the
/// reported offender is stable across runs. An empty provenance — content from
/// no file — is always [`Inspection::Allowed`].
///
/// ## Fail-closed on unknown provenance (REQ-544 C-1)
///
/// Content whose origin the daemon could not determine ([`Provenance::is_unknown`],
/// e.g. a `shell` result) is blocked whenever this inspector runs — the choke
/// point only inspects when boundaries are configured, so "unknown ⇒ blocked"
/// takes effect exactly when at least one boundary exists. The daemon cannot
/// prove the content is boundary-free, so it refuses to send it remotely rather
/// than gamble. The reported offender is the content-free
/// [`UNKNOWN_PROVENANCE_PATH`] sentinel (no real path, no file content).
///
/// ## Fail-closed on a malformed source (REQ-571 ADR-D)
///
/// [`first_malformed_source`] runs **before** boundary matching, so a source not
/// in canonical form is refused rather than matched — a distinction that matters
/// because a malformed source matches *no* glob and would therefore be allowed
/// through, which is the failure direction that leaks. The choke point runs the
/// same check itself, unconditionally and independent of whether any boundary is
/// configured (see [`crate::egress::Egress::send`]); this arm is what makes the
/// pure decision function fail closed on its own, so a caller cannot obtain a
/// verdict that skipped the guard.
#[must_use]
pub fn inspect(
    provenance: &Provenance,
    matcher: &BoundaryMatcher<'_>,
    action: PrivacyAction,
) -> Inspection {
    if first_malformed_source(provenance).is_some() {
        return Inspection::Blocked(Violation {
            path: MALFORMED_PROVENANCE_PATH.to_owned(),
            action,
        });
    }
    if provenance.is_unknown() {
        return Inspection::Blocked(Violation {
            path: UNKNOWN_PROVENANCE_PATH.to_owned(),
            action,
        });
    }
    for source in provenance.sources() {
        // Any matching boundary forbids egress in the MVP (fail-closed on every
        // mode; see the module docs). `match_path` already applies declaration
        // -order precedence and gitignore-style glob semantics.
        if matcher.match_path(source).is_some() {
            return Inspection::Blocked(Violation {
                path: source.to_owned(),
                action,
            });
        }
    }
    Inspection::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_core::ProvenanceId;

    fn boundary(glob: &str, mode: BoundaryMode) -> PrivacyBoundary {
        PrivacyBoundary {
            path_glob: glob.to_owned(),
            mode,
        }
    }

    fn matcher(boundaries: &[PrivacyBoundary]) -> BoundaryMatcher<'_> {
        BoundaryMatcher::new(boundaries).expect("valid globs")
    }

    #[test]
    fn empty_provenance_is_always_allowed() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let out = inspect(&Provenance::empty(), &m, PrivacyAction::ReroutedToLocal);
        assert_eq!(out, Inspection::Allowed);
    }

    #[test]
    fn provenance_outside_every_boundary_is_allowed() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("src/main.rs"))
            .union(&Provenance::tainted_by(fixture_id("README.md")));
        assert_eq!(
            inspect(&prov, &m, PrivacyAction::ReroutedToLocal),
            Inspection::Allowed
        );
    }

    #[test]
    fn a_local_only_source_is_blocked_with_its_path() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("secrets/prod.env"));
        let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
        let v = out.violation().expect("blocked");
        assert_eq!(v.path, "secrets/prod.env");
        assert_eq!(v.action, PrivacyAction::ReroutedToLocal);
    }

    #[test]
    fn one_boundary_source_among_many_safe_ones_still_blocks() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("src/a.rs"))
            .union(&Provenance::tainted_by(fixture_id("secrets/leak.txt")))
            .union(&Provenance::tainted_by(fixture_id("src/b.rs")));
        let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
        assert_eq!(
            out.violation().map(|v| v.path.as_str()),
            Some("secrets/leak.txt")
        );
    }

    #[test]
    fn redact_then_remote_is_also_blocked_until_the_redactor_exists() {
        // Fail-closed: the *substituting* redactor this mode needs is post-MVP
        // (OQ-7), so this content is held local rather than sent un-redacted.
        // REQ-562's `redact` detects and blocks — BR-5 forbids it rewriting a
        // payload — so it is not the redactor that would unblock this arm.
        let bs = vec![boundary("vendor/**", BoundaryMode::RedactThenRemote)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("vendor/private.json"));
        assert!(inspect(&prov, &m, PrivacyAction::ReroutedToLocal).is_blocked());
    }

    #[test]
    fn the_reported_offender_is_deterministic() {
        // Two boundary sources present; sorted iteration means the lexically
        // first is always reported, run after run.
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("secrets/z.env"))
            .union(&Provenance::tainted_by(fixture_id("secrets/a.env")));
        for _ in 0..8 {
            let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
            assert_eq!(
                out.violation().map(|v| v.path.as_str()),
                Some("secrets/a.env")
            );
        }
    }

    #[test]
    fn action_is_carried_through_to_the_violation() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("secrets/x"));
        let out = inspect(&prov, &m, PrivacyAction::Stripped);
        assert_eq!(out.violation().unwrap().action, PrivacyAction::Stripped);
    }

    #[test]
    fn no_boundaries_configured_allows_everything() {
        let bs: Vec<PrivacyBoundary> = Vec::new();
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("anything/at/all"));
        assert_eq!(
            inspect(&prov, &m, PrivacyAction::ReroutedToLocal),
            Inspection::Allowed
        );
    }

    #[test]
    fn unknown_provenance_is_blocked_fail_closed() {
        // REQ-544 C-1: content of indeterminate origin (a `shell` result) is
        // blocked when boundaries are configured, reported against the
        // content-free sentinel path.
        use crate::egress::provenance::UNKNOWN_PROVENANCE_PATH;
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let out = inspect(&Provenance::unknown(), &m, PrivacyAction::ReroutedToLocal);
        let v = out
            .violation()
            .expect("unknown provenance must fail closed");
        assert_eq!(v.path, UNKNOWN_PROVENANCE_PATH);
        assert_eq!(v.action, PrivacyAction::ReroutedToLocal);
    }

    // -----------------------------------------------------------------------
    // REQ-571 ADR-D — the malformed-source guard
    //
    // Every test below drives a value the type system says cannot exist, via
    // `ProvenanceId::unvalidated_for_test`. That is the point rather than a
    // workaround: ADR-A makes this guard unreachable from any typed path in the
    // daemon, and LESSON-508 is that a redundant guard *without* its own test
    // reads as dead code and gets deleted in the next cleanup — taking with it
    // the only thing standing between a future third-party-asserted source and
    // a boundary match that silently passes. These tests are the reason the
    // guard survives a refactor that "notices" it can never fire.
    // -----------------------------------------------------------------------

    /// A malformed source that names a boundary file must not be *allowed* just
    /// because no glob matches its spelling. This is the leak the guard exists
    /// to stop, so it is asserted against a matcher that would have passed it.
    #[test]
    fn a_malformed_source_naming_a_boundary_file_is_refused_not_matched() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        // The absolute spelling of a file the boundary covers. `secrets/**`
        // matches none of it — which is exactly why allowing it would leak.
        let prov =
            Provenance::tainted_by(ProvenanceId::unvalidated_for_test("/repo/secrets/prod.env"));
        let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
        let v = out
            .violation()
            .expect("a malformed source must fail closed");
        assert_eq!(v.path, MALFORMED_PROVENANCE_PATH);
        assert_eq!(v.action, PrivacyAction::ReroutedToLocal);
    }

    /// AC-5. The two shapes ADR-D names, **with no boundary configured** — the
    /// guard is not a boundary check and does not wait for one.
    #[test]
    fn an_absolute_and_a_traversing_source_are_refused_with_no_boundary_configured() {
        let bs: Vec<PrivacyBoundary> = Vec::new();
        let m = matcher(&bs);
        // Control: a well-formed source is allowed by this same empty matcher,
        // so the assertions below cannot pass because *everything* is blocked.
        assert_eq!(
            inspect(
                &Provenance::tainted_by(fixture_id("src/main.rs")),
                &m,
                PrivacyAction::ReroutedToLocal
            ),
            Inspection::Allowed
        );

        for source in ["/etc/passwd", "sub/../../etc/passwd"] {
            let prov = Provenance::tainted_by(ProvenanceId::unvalidated_for_test(source));
            let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
            assert_eq!(
                out.violation().map(|v| v.path.as_str()),
                Some(MALFORMED_PROVENANCE_PATH),
                "{source:?} must be refused with no boundary configured"
            );
        }
    }

    /// Each way of breaking canonical form reports its own reason: they are
    /// different problems with different fixes, and a collapsed reason would
    /// make the event unable to say which one happened.
    #[test]
    fn every_canonical_form_rule_reports_its_own_reason() {
        let cases = [
            ("/etc/passwd", ProvenanceRejection::Absolute),
            ("\\etc\\passwd", ProvenanceRejection::Absolute),
            ("../outside", ProvenanceRejection::ParentTraversal),
            ("sub\\..\\outside", ProvenanceRejection::ParentTraversal),
            ("./secrets/prod.env", ProvenanceRejection::NotCanonical),
            ("secrets//prod.env", ProvenanceRejection::NotCanonical),
            ("secrets/prod.env/", ProvenanceRejection::NotCanonical),
            ("", ProvenanceRejection::Empty),
        ];
        for (source, expected) in cases {
            let prov = Provenance::tainted_by(ProvenanceId::unvalidated_for_test(source));
            let found = first_malformed_source(&prov)
                .unwrap_or_else(|| panic!("{source:?} must be refused"));
            assert_eq!(found.reason, expected, "wrong reason for {source:?}");
        }
    }

    /// Non-vacuity for the guard itself: the canonical spellings a real mint
    /// produces pass it untouched, so it is refusing a shape rather than
    /// refusing everything.
    #[test]
    fn a_canonical_source_is_not_malformed() {
        for source in ["a", "secrets/prod.env", "a/b/c/d.rs", "x.y"] {
            let prov = Provenance::tainted_by(fixture_id(source));
            assert_eq!(
                first_malformed_source(&prov),
                None,
                "{source:?} is canonical and must pass the guard"
            );
        }
    }

    /// The load-bearing agreement between the two canonical-form checkers that
    /// live in different crates (`teton_core::provenance_id::mint`, reached here
    /// via the real `ProvenanceId::claimed` constructor, and this module's
    /// `malformed_reason`): whatever a genuine mint *produces* MUST pass the
    /// guard. The two are deliberately asymmetric — `mint` elides `.`/empty
    /// segments on input while the guard rejects them on output — so the
    /// invariant is one-directional (mint-output ⊆ guard-accepts), NOT that the
    /// functions are identical. Non-canonical inputs that mint normalizes away
    /// are exactly where a naive "assert both agree" test would wrongly fail.
    /// This is what stops the prose claim in `malformed_reason`'s doc from
    /// silently drifting into a lie if either function's rules change.
    #[test]
    fn every_minted_identity_passes_the_guard() {
        // Non-canonical but mintable spellings (no `..`, which `claimed` refuses)
        // — each normalizes to the same canonical id, which the guard must accept.
        for input in [
            "secrets/prod.env",
            "./secrets/prod.env",
            ".//secrets/prod.env",
            "secrets/./prod.env",
            "a/b//c",
        ] {
            let id = ProvenanceId::claimed(input)
                .unwrap_or_else(|e| panic!("{input:?} should mint, got {e:?}"));
            let prov = Provenance::tainted_by(id.clone());
            assert_eq!(
                first_malformed_source(&prov),
                None,
                "mint produced {:?} from {input:?}, but the guard rejected it — \
                 the two canonical-form checkers have drifted",
                id.as_str()
            );
        }
    }

    /// The reported source is sanitized before it leaves: a hostile spelling
    /// cannot smuggle a newline or an escape sequence into whatever renders the
    /// `provenance_rejected` event.
    #[test]
    fn the_reported_source_carries_no_control_characters() {
        let prov = Provenance::tainted_by(ProvenanceId::unvalidated_for_test(
            "/etc/\u{1b}[31mpasswd\nprivacy: all clear",
        ));
        let found = first_malformed_source(&prov).expect("refused");
        assert!(!found.source.contains('\n'), "{:?}", found.source);
        assert!(!found.source.contains('\u{1b}'), "{:?}", found.source);
        assert!(found.source.starts_with("/etc/"), "{:?}", found.source);
    }

    /// One malformed source among well-formed ones still refuses the whole
    /// provenance — the guard is a property of the set, not of a lucky
    /// iteration order.
    #[test]
    fn one_malformed_source_among_canonical_ones_still_refuses() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by(fixture_id("src/a.rs"))
            .union(&Provenance::tainted_by(ProvenanceId::unvalidated_for_test(
                "/etc/passwd",
            )))
            .union(&Provenance::tainted_by(fixture_id("src/b.rs")));
        assert_eq!(
            inspect(&prov, &m, PrivacyAction::ReroutedToLocal)
                .violation()
                .map(|v| v.path.as_str()),
            Some(MALFORMED_PROVENANCE_PATH)
        );
    }

    #[test]
    fn unknown_provenance_blocks_even_alongside_only_safe_sources() {
        // A context that read a public file AND ran a shell command: the unknown
        // shell contribution alone is enough to fail closed.
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let mut prov = Provenance::tainted_by(fixture_id("src/main.rs"));
        prov.mark_unknown();
        assert!(inspect(&prov, &m, PrivacyAction::ReroutedToLocal).is_blocked());
    }
}
