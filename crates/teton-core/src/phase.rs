//! The ADLC lifecycle [`Phase`] (decision D-4).
//!
//! Phase is the axis workflow-aware routing turns on (BR-5): rather than
//! guessing task difficulty from prompt text, the harness routes by lifecycle
//! phase. The variants mirror REQ-544's System Model enum exactly.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A lifecycle phase in structured (ADLC) mode, plus the catch-all `Freeform`
/// pseudo-phase used when a session is not driven by the ADLC gates.
///
/// The serialized form is the lowercase variant name (`"spec"`, `"architect"`,
/// …, `"io"`, `"freeform"`) so it round-trips through the config TOML and the
/// client protocol unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Requirement authoring — routed to a frontier model.
    Spec,
    /// Architecture and task decomposition — routed to a frontier model.
    Architect,
    /// Implementation from task-file artifacts — routed to a cheap/mid model.
    Implement,
    /// Code review — routed to a frontier model.
    Review,
    /// Mechanical I/O (summaries, grep triage, commit messages) — routed local.
    Io,
    /// Not in structured mode; heuristic routing applies (still emits a reason).
    Freeform,
}

impl Phase {
    /// Every phase, in lifecycle order. Handy for exhaustive, table-driven
    /// tests and for rendering the routing-policy table to the user.
    pub const ALL: [Phase; 6] = [
        Phase::Spec,
        Phase::Architect,
        Phase::Implement,
        Phase::Review,
        Phase::Io,
        Phase::Freeform,
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
            Phase::Freeform => "freeform",
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
        assert_eq!(Phase::ALL.len(), 6);
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
        assert_eq!(Phase::Freeform.to_string(), "freeform");
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
