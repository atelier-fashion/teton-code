//! Named permission levels — the level's shared *identity* (REQ-560).
//!
//! A [`PermissionLevel`] is a name for a per-tool policy posture. It lives here,
//! in the crate both the daemon and every client depend on, because three
//! different places need to agree about it and none of them may hold a second
//! copy:
//!
//! - the **daemon** expands a level into the policy table its gate enforces,
//! - the **client** renders the level in the entry frame's status row and lets
//!   the user change it with `/permissions`,
//! - the **wire** carries it, in both directions, on `session/permissions`.
//!
//! ## What is here and what is deliberately not
//!
//! This module owns the level's identity: its name, how a typed word becomes
//! one, the one line `/permissions` prints about it, and the sentence a call
//! denied *by the level* returns. It does **not** own the policy table. That is
//! `tetond`'s `table_for`, and the split is not squeamishness about layering —
//! `PermissionConfig` is what the gate enforces, not what the wire carries, and
//! a table on the wire is a table a client could try to send. A client may name
//! a posture; it may never describe one.
//!
//! ## One classifier (REQ-560 BR-15)
//!
//! The level a session is in, the table it expands to, and the sentence a denied
//! call returns all derive from a single enum through total functions:
//! [`PermissionLevel::name`], `tetond`'s `table_for`, and
//! [`PermissionLevel::denial_sentence`]. Every function here is an **exhaustive
//! match with no `_` arm**, which is what makes the compiler — rather than a
//! reviewer's memory — the thing that stops a fifth level from shipping with
//! four surfaces filled in. Two surfaces describing one permission state must
//! not be able to drift (LESSON-456).
//!
//! ## Levels are not an egress control (REQ-560 BR-3)
//!
//! Nothing here touches egress. A level governs **which tools may run**; the
//! privacy boundary governs **what leaves the machine**. [`PermissionLevel::Full`]
//! grants tool execution and leaves the `local-only` boundary and the
//! session-taint pin exactly where they were. Wiring the two together — even as
//! a convenience — would turn REQ-544's flagship guarantee into a setting
//! (LESSON-432).

use serde::{Deserialize, Serialize};

/// How permissive a session is about running tools.
///
/// Session-scoped: `/permissions <level>` changes it for the current session and
/// persists nothing, so every new session starts at the configured
/// `default_permission_level` (REQ-560 BR-6). That asymmetry with REQ-559's
/// persisted reasoning effort is deliberate — an effort level that survives a
/// restart costs money predictably, while a `Full` that survives a restart
/// removes a guardrail invisibly, in a session the user does not remember
/// configuring.
///
/// ACP: no counterpart. ACP has permission *requests* but no named posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    /// Reads auto-allow; `edit` and `shell` ask. The default, and byte-equal to
    /// the daemon's pre-REQ-560 `coding_defaults()`.
    Guarded,
    /// `edit` auto-allows; `shell` still asks. The posture most users live in.
    Edits,
    /// Every mutating tool **denies** rather than asking — read-only tools only.
    ///
    /// Denial rather than prompting is the whole point: a denied call tells the
    /// model and forbids a retry, which produces a plan. A prompt storm produces
    /// frustration (REQ-560 BR-2).
    Plan,
    /// Everything auto-allows, and byte-equal to the daemon's pre-REQ-560
    /// `permissive()`.
    ///
    /// Grants tool *execution* only. It does not touch egress — see the module
    /// docs.
    Full,
}

impl PermissionLevel {
    /// Every level, in the order `/permissions` lists them: loosest-to-strictest
    /// is *not* the order, because `plan` is not "stricter than guarded" so much
    /// as a different question. This is the order the spec's table uses.
    ///
    /// Iterating this is how a test covers every surface without enumerating
    /// variants by hand (REQ-560 AC-17): a fifth level joins the array once and
    /// every `ALL`-driven assertion picks it up.
    pub const ALL: &'static [Self] = &[Self::Guarded, Self::Edits, Self::Plan, Self::Full];

    /// The level's name — the word the user types and the word the status row
    /// shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Guarded => "guarded",
            Self::Edits => "edits",
            Self::Plan => "plan",
            Self::Full => "full",
        }
    }

    /// The level a typed word names, or `None`.
    ///
    /// Matched **exactly**, against the closed set. No prefix matching, no case
    /// folding, no nearest-neighbour guess — for the reason the slash-command
    /// classifier gives about lenient matching: two spellings reaching one
    /// handler means the one the user did not intend is the one nobody tests.
    /// Here it is sharper still, because the thing being named decides whether a
    /// shell command runs without asking.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|level| level.name() == s)
    }

    /// The one line `/permissions` prints about this level.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Guarded => "reads run freely; edits and shell commands ask first",
            Self::Edits => "reads and edits run freely; shell commands ask first",
            Self::Plan => "read-only — every tool that would change something is refused",
            Self::Full => "every tool runs without asking (does not affect privacy boundaries)",
        }
    }

    /// The sentence a tool call denied **by this level** returns to the model.
    ///
    /// Distinct from the denial a user types, and the distinction matters: "the
    /// user declined" is wrong when nobody was asked, and a model told the wrong
    /// reason proposes the wrong remedy. The no-retry clause is retained from the
    /// user-declined sentence because it is doing the same job in both — the call
    /// is settled, and re-proposing it wastes a turn (REQ-560 BR-2).
    ///
    /// The client renders this same sentence to the *user* when a tool is refused
    /// by the level, from this same function, so the two cannot drift (BR-15).
    #[must_use]
    pub fn denial_sentence(self, tool: &str) -> String {
        format!(
            "Permission denied: the `{}` permission level does not allow `{tool}` ({}). \
             Do not retry this tool; take a different approach or finish.",
            self.name(),
            self.summary(),
        )
    }
}

impl Default for PermissionLevel {
    /// `guarded` — the value a session starts at when nothing says otherwise
    /// (REQ-560 System Model, `default_permission_level`).
    fn default() -> Self {
        Self::Guarded
    }
}

impl std::fmt::Display for PermissionLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every assertion below drives off [`PermissionLevel::ALL`] rather than
    /// naming variants, so a fifth level is covered the moment it joins the
    /// array — REQ-560 AC-17's "adding a level exercises every surface without
    /// touching a second table", in the form Rust can actually enforce.
    #[test]
    fn every_level_round_trips_through_its_name() {
        for level in PermissionLevel::ALL {
            assert_eq!(
                PermissionLevel::parse(level.name()),
                Some(*level),
                "{} did not round-trip",
                level.name()
            );
        }
    }

    #[test]
    fn all_holds_each_variant_exactly_once() {
        let mut names: Vec<&str> = PermissionLevel::ALL.iter().map(|l| l.name()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "a level appears twice in ALL");
        assert_eq!(before, 4, "ALL must list every level");
    }

    #[test]
    fn parse_refuses_anything_that_is_not_an_exact_name() {
        for bad in [
            "", "Guarded", "GUARDED", "guard", "guarded ", " guarded", "all", "none", "plan!",
        ] {
            assert_eq!(
                PermissionLevel::parse(bad),
                None,
                "`{bad}` must not name a level"
            );
        }
    }

    #[test]
    fn every_level_has_a_summary_and_a_denial_sentence() {
        for level in PermissionLevel::ALL {
            assert!(!level.summary().is_empty(), "{level} has no summary");

            let sentence = level.denial_sentence("edit");
            assert!(
                sentence.contains(level.name()),
                "{level}'s denial sentence must name the level: {sentence}"
            );
            assert!(
                sentence.contains("`edit`"),
                "{level}'s denial sentence must name the tool: {sentence}"
            );
            // BR-2: the model must be told not to retry, whatever the level.
            assert!(
                sentence.contains("Do not retry this tool"),
                "{level}'s denial sentence must forbid a retry: {sentence}"
            );
        }
    }

    #[test]
    fn the_default_level_is_guarded() {
        assert_eq!(PermissionLevel::default(), PermissionLevel::Guarded);
    }

    #[test]
    fn every_level_serialises_as_its_lowercase_name() {
        for level in PermissionLevel::ALL {
            let json = serde_json::to_string(level).expect("level serialises");
            assert_eq!(json, format!("\"{}\"", level.name()));
            let back: PermissionLevel = serde_json::from_str(&json).expect("level deserialises");
            assert_eq!(back, *level);
        }
    }

    #[test]
    fn an_unknown_wire_value_is_a_deserialisation_error_not_a_default() {
        // A level the daemon does not know must fail loudly rather than quietly
        // becoming `guarded` — a posture nobody chose is the shape of a guard
        // that silently stops guarding.
        assert!(serde_json::from_str::<PermissionLevel>("\"unrestricted\"").is_err());
    }

    #[test]
    fn display_matches_name() {
        for level in PermissionLevel::ALL {
            assert_eq!(level.to_string(), level.name());
        }
    }
}
