//! The ADLC lifecycle [`Phase`] (decision D-4).
//!
//! Phase is **not** a routing input (REQ-558 BR-1/AC-9): what a model call is
//! *for* — its [`crate::Category`] — decides which provider serves it. Phase
//! survives as the two facts it is genuinely good for: which ADLC gate a
//! structured session sits behind, and which lifecycle bucket a cost row rolls
//! up into (BR-11). The variants mirror REQ-544's System Model enum minus the
//! retired `Freeform` pseudo-phase (ADR-G).
//!
//! No router signature takes a `Phase` any more. A structured turn maps its
//! phase to a category with [`crate::category_for_phase`] and hands the router
//! the *category*; the phase is stamped back onto the decision afterwards, for
//! cost attribution only (AC-9).

use serde::{Deserialize, Serialize};
use std::fmt;

/// A lifecycle phase in structured (ADLC) mode.
///
/// There is no `Freeform` variant: freeform is a session *mode*
/// (`Session.mode`), not a position in the lifecycle, and a freeform session
/// carries no phase at all — every phase-bearing field in the workspace is
/// already `Option<Phase>` and freeform turns have always stored `None`
/// (ADR-G).
///
/// The serialized form is the lowercase variant name (`"spec"`, `"architect"`,
/// …, `"io"`) so it round-trips through the config TOML and the client protocol
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Requirement authoring. Dispatches on [`crate::Category::Design`].
    Spec,
    /// Architecture and task decomposition. Dispatches on
    /// [`crate::Category::Design`].
    Architect,
    /// Implementation from task-file artifacts. Dispatches on
    /// [`crate::Category::Edit`].
    Implement,
    /// Code review. Dispatches on [`crate::Category::Review`].
    Review,
    /// Mechanical I/O (summaries, grep triage, commit messages). Dispatches on
    /// [`crate::Category::Digest`].
    Io,
}

impl Phase {
    /// Every phase, in lifecycle order. Handy for exhaustive, table-driven
    /// tests and for rendering per-phase cost rollups to the user.
    pub const ALL: [Phase; 5] = [
        Phase::Spec,
        Phase::Architect,
        Phase::Implement,
        Phase::Review,
        Phase::Io,
    ];

    /// The lowercase wire/display name of this phase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Spec => "spec",
            Phase::Architect => "architect",
            Phase::Implement => "implement",
            Phase::Review => "review",
            Phase::Io => "io",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_contains_every_variant_once() {
        // Five, not six: ADR-G retires `Freeform`. Freeform is a session mode,
        // and a mode is not a lifecycle position.
        assert_eq!(Phase::ALL.len(), 5);
        // Uniqueness: no variant repeated.
        for (i, a) in Phase::ALL.iter().enumerate() {
            for b in &Phase::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn display_matches_lowercase_wire_name() {
        assert_eq!(Phase::Spec.to_string(), "spec");
        assert_eq!(Phase::Architect.to_string(), "architect");
        assert_eq!(Phase::Implement.to_string(), "implement");
        assert_eq!(Phase::Review.to_string(), "review");
        assert_eq!(Phase::Io.to_string(), "io");
    }

    #[test]
    fn the_retired_freeform_variant_has_no_way_back_in() {
        // ADR-G: `"freeform"` is not a phase any more, at any layer. Serde is
        // the outermost gate — a config or a client payload naming it fails to
        // deserialize rather than landing on some substitute phase.
        #[derive(Debug, Deserialize)]
        struct Wrap {
            #[allow(dead_code)]
            phase: Phase,
        }
        let err = toml::from_str::<Wrap>("phase = \"freeform\"\n")
            .expect_err("`freeform` must not deserialize into a Phase");
        assert!(
            err.to_string().contains("freeform"),
            "the error should name the rejected value: {err}"
        );
    }

    #[test]
    fn serde_round_trips_through_json_like_string() {
        for phase in Phase::ALL {
            // serde_json is not a dependency; use toml via a wrapper table.
            #[derive(Serialize, Deserialize, PartialEq, Debug)]
            struct Wrap {
                phase: Phase,
            }
            let s = toml::to_string(&Wrap { phase }).unwrap();
            assert!(s.contains(phase.as_str()));
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.phase, phase);
        }
    }
}
