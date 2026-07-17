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

use teton_core::boundary::BoundaryMatcher;
use teton_protocol::events::PrivacyAction;

use crate::egress::provenance::Provenance;

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

/// Inspect `provenance` against `matcher`, taking `action` when a boundary is
/// hit.
///
/// Returns [`Inspection::Blocked`] for the first (lowest, deterministic) source
/// path that any boundary covers. Sources are iterated in sorted order so the
/// reported offender is stable across runs. An empty provenance — content from
/// no file — is always [`Inspection::Allowed`].
#[must_use]
pub fn inspect(
    provenance: &Provenance,
    matcher: &BoundaryMatcher<'_>,
    action: PrivacyAction,
) -> Inspection {
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
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};

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
        let prov =
            Provenance::tainted_by("src/main.rs").union(&Provenance::tainted_by("README.md"));
        assert_eq!(
            inspect(&prov, &m, PrivacyAction::ReroutedToLocal),
            Inspection::Allowed
        );
    }

    #[test]
    fn a_local_only_source_is_blocked_with_its_path() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by("secrets/prod.env");
        let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
        let v = out.violation().expect("blocked");
        assert_eq!(v.path, "secrets/prod.env");
        assert_eq!(v.action, PrivacyAction::ReroutedToLocal);
    }

    #[test]
    fn one_boundary_source_among_many_safe_ones_still_blocks() {
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by("src/a.rs")
            .union(&Provenance::tainted_by("secrets/leak.txt"))
            .union(&Provenance::tainted_by("src/b.rs"));
        let out = inspect(&prov, &m, PrivacyAction::ReroutedToLocal);
        assert_eq!(
            out.violation().map(|v| v.path.as_str()),
            Some("secrets/leak.txt")
        );
    }

    #[test]
    fn redact_then_remote_is_also_blocked_until_the_redactor_exists() {
        // Fail-closed: the redactor is post-MVP (OQ-7), so this content is held
        // local rather than sent un-redacted.
        let bs = vec![boundary("vendor/**", BoundaryMode::RedactThenRemote)];
        let m = matcher(&bs);
        let prov = Provenance::tainted_by("vendor/private.json");
        assert!(inspect(&prov, &m, PrivacyAction::ReroutedToLocal).is_blocked());
    }

    #[test]
    fn the_reported_offender_is_deterministic() {
        // Two boundary sources present; sorted iteration means the lexically
        // first is always reported, run after run.
        let bs = vec![boundary("secrets/**", BoundaryMode::LocalOnly)];
        let m = matcher(&bs);
        let prov =
            Provenance::tainted_by("secrets/z.env").union(&Provenance::tainted_by("secrets/a.env"));
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
        let prov = Provenance::tainted_by("secrets/x");
        let out = inspect(&prov, &m, PrivacyAction::Stripped);
        assert_eq!(out.violation().unwrap().action, PrivacyAction::Stripped);
    }

    #[test]
    fn no_boundaries_configured_allows_everything() {
        let bs: Vec<PrivacyBoundary> = Vec::new();
        let m = matcher(&bs);
        let prov = Provenance::tainted_by("anything/at/all");
        assert_eq!(
            inspect(&prov, &m, PrivacyAction::ReroutedToLocal),
            Inspection::Allowed
        );
    }
}
