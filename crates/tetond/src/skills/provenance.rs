//! The fold: a skill's identity plus its preambles' verdicts become one
//! provenance (REQ-619 BR-2, ADR-619-4).
//!
//! Two call sites used to answer this question, and they answered it
//! differently. The typed `/name` path OR'd `outcomes.iter().any(spawned)` into
//! `SkillTurn::unknown`; the model's `skill` tool matched `(source, spawned)`
//! and mapped `(_, true) | (User, _)` to [`ToolProvenance::Unknown`]. Neither
//! read a verdict, because until REQ-619 a preamble had none. Now it has one
//! ([`super::dynamic::run_all`] takes it before each spawn), and this module is
//! the single place it becomes a provenance — so the two paths cannot come to
//! disagree again, and one set of unit tests covers both (BR-6).
//!
//! # Pure, and that is load-bearing
//!
//! Nothing here runs a command, reads a file or compiles a glob: it folds
//! values the runner already produced. That is what lets the typed path and the
//! model-invoked path share it, and what lets the table below be tested without
//! a daemon, a terminal or a temp directory (ADR-619-4).
//!
//! # What closes the exit-code side channel
//!
//! The **verdict**, not the outcome. `` !`grep -q AKIA secrets/prod.env && exit
//! 1 || exit 2` `` is a content-reading verb on a boundary path, so it is
//! `BoundaryTouch` before it spawns; this fold reads that verdict and never the
//! exit status, so the turn is refused whatever the command chose to exit with
//! (REQ-585 verify, BR-2). The outcome is read for exactly one bit — did the
//! command spawn at all — because a command the consent door declined
//! contributed nothing to the prompt and must contribute nothing here.

use std::collections::BTreeSet;

use teton_core::ProvenanceId;

use crate::harness::context::ToolProvenance;
use crate::harness::tools::VerdictKind;

use super::dynamic::{DynamicOutcome, PreambleRun};

/// What a skill expansion's text is worth to egress: the files it can name, and
/// the two things it cannot (REQ-619 ADR-619-4).
///
/// The same triple `harness::context::Provenance::User` carries, because it is
/// the same fact travelling: this is the value the seed block is built from and
/// the value the model-invoked tool result is tagged with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExpansionProvenance {
    /// Every identity this expansion can name: the skill file's own, plus each
    /// rooted preamble's resolved path arguments, plus the ids of an **in-root**
    /// boundary path — which mints, and so is matched by the glob that protects
    /// it exactly as a `read` of it would be.
    pub sources: BTreeSet<ProvenanceId>,
    /// Some contributing preamble's reach could not be proved, or the skill file
    /// itself had no identity to mint. Fail-closed at egress, liftable by
    /// `/shell allow` (REQ-614 BR-5).
    pub unknown: bool,
    /// Some contributing preamble named a boundary file **outside** the session
    /// root — no id exists for a glob to match, so a bit is the only carrier
    /// (LESSON-623). Fail-closed at egress and **not** liftable: the pin is
    /// permanent, because a path was actually named.
    pub boundary_touch: bool,
}

impl ExpansionProvenance {
    /// This expansion as the [`ToolProvenance`] a model-invoked `skill` result
    /// is tagged with (ADR-619-4's second consumer).
    ///
    /// The arms are `ShellTool::run`'s, in `ShellTool::run`'s order, because a
    /// skill expansion and a `shell` result mean the same thing to egress and
    /// two orderings of one mapping are how one of them comes to be laxer
    /// (REQ-614 BR-10). There the in-root boundary case is a `Sources` arm
    /// ahead of the sentinel; here [`fold_expansion`] has already folded those
    /// ids into `sources`, so what reaches this function is the out-of-root
    /// residue alone.
    ///
    /// # Why `boundary_touch` outranks `unknown`
    ///
    /// Both refuse the send, so the choice cannot make the turn laxer — it
    /// decides what the refusal **reports**. `ToolProvenance::BoundaryTouch`
    /// reports `<boundary-touch>` as the path, and `taint::cause_of` reads that
    /// path to record `boundary_hit` and hold the pin permanently; `Unknown`
    /// reports `<unknown-provenance>`, which `/shell allow` may lift (REQ-614
    /// ADR-614-3, BUG-215). An expansion that both read `~/.ssh/config` and ran
    /// an opaque verb has to keep the permanent cause, so the more specific
    /// reading goes first.
    ///
    /// `Sources` last, and it may be empty: an expansion with a minted identity
    /// and no preambles is a `read` of one file, and one with neither — a
    /// project skill that will not mint cannot happen — is content that names
    /// nothing and pins nothing.
    #[must_use]
    pub fn into_tool_provenance(self) -> ToolProvenance {
        if self.boundary_touch {
            ToolProvenance::BoundaryTouch
        } else if self.unknown {
            ToolProvenance::Unknown
        } else {
            ToolProvenance::Sources(self.sources)
        }
    }
}

/// Whether this command reached a process at all.
///
/// Deliberately **not** [`DynamicOutcome::spawned`], which is the pre-REQ-619
/// provenance rule's own predicate and is retired by TASK-401: a new decision
/// that read it would be a second answer to a question this module now owns,
/// and the day `spawned` is deleted or narrowed for the old rule's sake this
/// fold would move with it for no reason of its own. The two matches are
/// identical today and mean different things: that one asks *may this pin the
/// turn*, this one asks *did the door let it through*.
fn did_spawn(outcome: &DynamicOutcome) -> bool {
    match outcome {
        DynamicOutcome::Ran { .. } | DynamicOutcome::Failed { .. } | DynamicOutcome::TimedOut => {
            true
        }
        DynamicOutcome::NotRun { .. } => false,
    }
}

/// ADR-619-4's table, row for row.
///
/// | input | effect |
/// |---|---|
/// | `identity: Some(id)` | `sources ∪ {id}` |
/// | `identity: None` | `unknown = true` |
/// | `Rooted` verdict on a command that ran | `sources ∪ verdict.sources` |
/// | `BoundaryTouch` with sources (in-root) | `sources ∪ verdict.sources` |
/// | `BoundaryTouch` without sources (out-of-root) | `boundary_touch = true` |
/// | `Unknown` | `unknown = true` |
/// | any verdict on a `NotRun` command | nothing |
///
/// `identity` is `None` for a skill file that will not mint — after TASK-398
/// that is a file under neither the session root nor the home, and the answer
/// stays what it has always been: fail closed (REQ-585 ADR-9).
///
/// The `NotRun` row is BR-2's "a command that did not run contributes nothing".
/// A command declined at the consent door, or held back at `plan`, still
/// *carries* a verdict — [`super::dynamic::run_all`] classifies before it looks
/// at the door, because the verdict is a fact about the command text — but it
/// put no bytes in the prompt and read no file, so folding its verdict would
/// pin a turn on a command that never happened.
///
/// Every other row reads the verdict and only the verdict. No arm inspects
/// output, exit status or duration, which is what makes BR-2's "output never
/// changes the verdict" structural here rather than a claim about this
/// function's current body: a `Rooted` command that timed out folds exactly as
/// one that printed a page.
#[must_use]
pub fn fold_expansion(identity: Option<ProvenanceId>, runs: &[PreambleRun]) -> ExpansionProvenance {
    let mut folded = ExpansionProvenance::default();
    match identity {
        Some(id) => {
            folded.sources.insert(id);
        }
        None => folded.unknown = true,
    }
    for run in runs {
        if !did_spawn(&run.outcome) {
            continue;
        }
        match run.verdict.kind {
            VerdictKind::Rooted => folded.sources.extend(run.verdict.sources.iter().cloned()),
            // The in-root case carries its own minted ids, so egress refuses
            // naming the actual file — a `cat .env` preamble and a `read` of
            // `.env` produce the same `privacy_block`. Only the out-of-root case
            // has nothing to name, and that is what the bit is for (ADR-619-2).
            VerdictKind::BoundaryTouch if !run.verdict.sources.is_empty() => {
                folded.sources.extend(run.verdict.sources.iter().cloned());
            }
            VerdictKind::BoundaryTouch => folded.boundary_touch = true,
            VerdictKind::Unknown => folded.unknown = true,
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fixture_id;
    use crate::harness::tools::Verdict;

    /// A verdict of `kind` naming `sources`, with the reason every classifier
    /// answer carries. The reason is never read by the fold — it exists for the
    /// event and the daemon's stderr — so one literal covers every row.
    fn verdict(kind: VerdictKind, sources: &[&str]) -> Verdict {
        Verdict {
            kind,
            sources: sources.iter().map(|s| fixture_id(s)).collect(),
            reason: "fixture",
        }
    }

    /// A command that ran and printed something.
    fn ran() -> DynamicOutcome {
        DynamicOutcome::Ran {
            output: "out".to_owned(),
            fell_back: false,
            truncated: false,
        }
    }

    /// A command that ran and chose its own exit code — the AC-6 shape.
    fn exited(code: i32) -> DynamicOutcome {
        DynamicOutcome::Failed {
            status: format!("exited {code}"),
            exit_status: Some(code),
        }
    }

    fn run(kind: VerdictKind, sources: &[&str], outcome: DynamicOutcome) -> PreambleRun {
        PreambleRun {
            verdict: verdict(kind, sources),
            outcome,
        }
    }

    fn ids(prov: &ExpansionProvenance) -> Vec<String> {
        prov.sources
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect()
    }

    /// **BR-2 / ADR-619-4, row for row.**
    ///
    /// Table-driven because the ADR is a table: a row that is not here is a row
    /// nothing checks, and the two are read side by side. Each case names the
    /// row it comes from.
    ///
    /// # Benign path
    ///
    /// The first three rows are the ones a proportionate fold has to keep
    /// *clean*: a project skill with two `cat`s of in-root files ends with three
    /// named ids, neither bit set, and reaches the wire. A fold that pinned
    /// everything would pass a "does it refuse" test and fail these.
    ///
    /// # AC-6, in the last two rows
    ///
    /// A `BoundaryTouch` command that exited 2 folds byte-identically to one
    /// that exited 0. The verdict is taken before the spawn, so the exit code
    /// the command **chose** — one bit per command about a `local-only` file,
    /// the side channel REQ-585's verify found — reaches nothing.
    ///
    /// # Mutation
    ///
    /// Ran with `VerdictKind::BoundaryTouch if !run.verdict.sources.is_empty()`
    /// deleted from `fold_expansion`, so an in-root boundary path falls through
    /// to the bit: **two red**, this test on `an in-root boundary path is named,
    /// not a bare bit` (sources short by one — `secrets/prod.env` is gone from
    /// the set, and the block would have been refused against `<boundary-touch>`
    /// instead of naming the file) and the sibling below on its non-vacuity
    /// half. Restored: green. Ran again with `did_spawn` inverted: two red
    /// again. The `did_spawn` guard merely *deleted* reddens the sibling alone —
    /// which is why the sibling exists.
    #[test]
    fn the_fold_follows_the_adr_table() {
        let skill = fixture_id(".claude/skills/release/SKILL.md");

        // Row: `identity: Some(id)`.
        let only_identity = fold_expansion(Some(skill.clone()), &[]);
        assert_eq!(ids(&only_identity), [".claude/skills/release/SKILL.md"]);
        assert!(!only_identity.unknown, "a minted skill file pins nothing");
        assert!(!only_identity.boundary_touch);

        // Row: `identity: None`.
        let no_identity = fold_expansion(None, &[]);
        assert!(
            no_identity.unknown,
            "a file with no identity to mint fails closed"
        );
        assert!(!no_identity.boundary_touch, "and it names no path either");
        assert!(ids(&no_identity).is_empty());

        // Row: `Rooted` on a command that ran — the AC-3 shape, and the one a
        // proportionate fold exists for.
        let rooted = fold_expansion(
            Some(skill.clone()),
            &[
                run(VerdictKind::Rooted, &["README.md"], ran()),
                run(VerdictKind::Rooted, &["docs/design.md"], ran()),
            ],
        );
        assert_eq!(
            ids(&rooted),
            [
                ".claude/skills/release/SKILL.md",
                "README.md",
                "docs/design.md"
            ]
        );
        assert!(!rooted.unknown, "two in-root cats must not pin the session");
        assert!(!rooted.boundary_touch);

        // Row: `BoundaryTouch` **with** sources — an in-root boundary path
        // names itself, exactly as a `read` of it would.
        let in_root = fold_expansion(
            Some(skill.clone()),
            &[run(
                VerdictKind::BoundaryTouch,
                &["secrets/prod.env"],
                ran(),
            )],
        );
        assert_eq!(
            ids(&in_root),
            [".claude/skills/release/SKILL.md", "secrets/prod.env"],
            "an in-root boundary path is named, not a bare bit"
        );
        assert!(
            !in_root.boundary_touch,
            "the sentinel is for the out-of-root case alone"
        );
        assert!(!in_root.unknown);

        // Row: `BoundaryTouch` **without** sources — `cat ~/.ssh/config`, which
        // mints nothing for a glob to match.
        let out_of_root = fold_expansion(
            Some(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], ran())],
        );
        assert!(out_of_root.boundary_touch, "the bit is the only carrier");
        assert_eq!(ids(&out_of_root), [".claude/skills/release/SKILL.md"]);

        // Row: `Unknown` — an opaque verb, liftable.
        let unknown = fold_expansion(
            Some(skill.clone()),
            &[run(VerdictKind::Unknown, &[], ran())],
        );
        assert!(unknown.unknown);
        assert!(
            !unknown.boundary_touch,
            "an unprovable command names no path, so its pin stays liftable"
        );

        // AC-6: the exit code the command chose changes nothing. Same verdict,
        // three outcomes, one answer.
        let exit_zero = fold_expansion(
            Some(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], ran())],
        );
        let exit_one = fold_expansion(
            Some(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], exited(1))],
        );
        let exit_two = fold_expansion(
            Some(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], exited(2))],
        );
        assert_eq!(
            exit_one, exit_zero,
            "a boundary touch that exited 1 must fold as one that exited 0"
        );
        assert_eq!(
            exit_two, exit_zero,
            "the exit-code side channel is closed by the verdict, not by output"
        );
        // And a timeout is the same channel with a sleep.
        assert_eq!(
            fold_expansion(
                Some(skill),
                &[run(
                    VerdictKind::BoundaryTouch,
                    &[],
                    DynamicOutcome::TimedOut
                )]
            ),
            exit_zero,
            "a killed command still touched the boundary it named"
        );
    }

    /// **BR-2's `NotRun` row.** A command the door declined contributed no bytes
    /// to the prompt and opened no file, so no verdict of its can pin the turn.
    ///
    /// All three kinds, because the row is about the *outcome* and a fold that
    /// special-cased one kind would pass a single-case test. The identity is
    /// `Some` throughout so the assertions are about the commands alone.
    ///
    /// # Benign path
    ///
    /// The last case is the same three verdicts on commands that **did** run,
    /// and it pins: a test that only proved "nothing pins" would also pass on a
    /// fold that had stopped reading verdicts at all (LESSON-640 — invert it and
    /// count).
    ///
    /// # Mutation
    ///
    /// Ran with `if !did_spawn(&run.outcome) { continue; }` deleted: **one red**,
    /// this test on `a declined command contributes nothing` — the four declined
    /// commands contributed `README.md` and `secrets/prod.env` to a turn in which
    /// nothing ran. Restored: green. Ran again with `did_spawn`'s two arms
    /// swapped (`NotRun` spawning and the rest not): red on this test and on
    /// `the_fold_follows_the_adr_table` both.
    #[test]
    fn an_unrun_command_contributes_nothing_whatever_its_verdict() {
        let skill = fixture_id(".claude/skills/release/SKILL.md");
        let declined = |kind, sources: &[&str]| {
            run(
                kind,
                sources,
                DynamicOutcome::NotRun {
                    reason: "not allowed by permission level".to_owned(),
                },
            )
        };

        let folded = fold_expansion(
            Some(skill.clone()),
            &[
                declined(VerdictKind::Rooted, &["README.md"]),
                declined(VerdictKind::BoundaryTouch, &["secrets/prod.env"]),
                declined(VerdictKind::BoundaryTouch, &[]),
                declined(VerdictKind::Unknown, &[]),
            ],
        );
        assert_eq!(
            ids(&folded),
            [".claude/skills/release/SKILL.md"],
            "a declined command contributes nothing"
        );
        assert!(!folded.unknown, "a declined command contributes nothing");
        assert!(
            !folded.boundary_touch,
            "a declined command contributes nothing"
        );
        assert_eq!(
            folded,
            fold_expansion(Some(skill.clone()), &[]),
            "four declined commands must fold as no commands at all"
        );

        // Non-vacuity: the same four verdicts on commands that ran say all three
        // things.
        let spawned = fold_expansion(
            Some(skill),
            &[
                run(VerdictKind::Rooted, &["README.md"], ran()),
                run(VerdictKind::BoundaryTouch, &["secrets/prod.env"], ran()),
                run(VerdictKind::BoundaryTouch, &[], ran()),
                run(VerdictKind::Unknown, &[], ran()),
            ],
        );
        assert_eq!(
            ids(&spawned),
            [
                ".claude/skills/release/SKILL.md",
                "README.md",
                "secrets/prod.env"
            ]
        );
        assert!(spawned.unknown);
        assert!(spawned.boundary_touch);
    }

    /// The second consumer's mapping (ADR-619-4), and its precedence.
    ///
    /// `ShellTool::run`'s arms in `ShellTool::run`'s order: the permanent cause
    /// first, then the liftable one, then the ids. An expansion carrying both
    /// bits must map to `BoundaryTouch` — mapping it to `Unknown` would refuse
    /// the same send today and make the pin liftable by `/shell allow`, which is
    /// the whole difference the bit exists to carry.
    ///
    /// # Mutation
    ///
    /// Ran with the first two arms swapped (`unknown` tested first): red on
    /// `an expansion that touched a boundary keeps the permanent cause`,
    /// `left: Unknown, right: BoundaryTouch` — the send still refused, the pin
    /// now liftable. Restored: green.
    #[test]
    fn the_tool_provenance_mapping_is_the_shell_tools() {
        let sources: BTreeSet<_> = [fixture_id("README.md")].into_iter().collect();

        assert_eq!(
            ExpansionProvenance {
                sources: sources.clone(),
                unknown: false,
                boundary_touch: false,
            }
            .into_tool_provenance(),
            ToolProvenance::Sources(sources.clone())
        );
        assert_eq!(
            ExpansionProvenance {
                sources: sources.clone(),
                unknown: true,
                boundary_touch: false,
            }
            .into_tool_provenance(),
            ToolProvenance::Unknown
        );
        assert_eq!(
            ExpansionProvenance {
                sources,
                unknown: true,
                boundary_touch: true,
            }
            .into_tool_provenance(),
            ToolProvenance::BoundaryTouch,
            "an expansion that touched a boundary keeps the permanent cause"
        );
    }
}
