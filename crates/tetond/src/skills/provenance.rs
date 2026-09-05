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

use super::discovery::SkillIdentity;
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
    /// The mapping is not written here. [`ToolProvenance::from_bits`] is the one
    /// precedence, shared with `ShellTool::run` and
    /// `ContextManager::compaction_summary` (REQ-619 verify, M3), because a
    /// skill expansion and a `shell` result mean the same thing to egress and
    /// three hand-written orderings of one mapping are how one of them comes to
    /// be laxer (REQ-614 BR-10). This function's whole job is to say that an
    /// [`ExpansionProvenance`]'s three fields *are* those three bits.
    ///
    /// # What the shared mapping does with them
    ///
    /// `boundary_touch` first: every arm refuses the send, so the order cannot
    /// make a turn laxer — it decides what the refusal **reports**.
    /// `BoundaryTouch` reports `<boundary-touch>`, which `taint::cause_of` reads
    /// to hold the pin permanently; `Unknown` reports `<unknown-provenance>`,
    /// which `/shell allow` may lift (REQ-614 ADR-614-3, BUG-215).
    ///
    /// Then the C1 arm, which is why the hand-written version had to go: an
    /// expansion that is `unknown` **and** proved ids maps to
    /// [`ToolProvenance::UnknownWith`], not to a bare `Unknown`. A skill whose
    /// preambles are `` !`sh -c 'echo hi'` `` and `` !`cat secrets/prod.env` ``
    /// folds to exactly that pair; collapsing it dropped `secrets/prod.env`, and
    /// then `/shell allow` cleared the opacity over an **empty** source set —
    /// a clean provenance, and the secret left the machine.
    ///
    /// `Sources` last, and it may be empty: an expansion with a minted identity
    /// and no preambles is a `read` of one file, and one with neither — a
    /// project skill that will not mint cannot happen — is content that names
    /// nothing and pins nothing.
    #[must_use]
    pub fn into_tool_provenance(self) -> ToolProvenance {
        ToolProvenance::from_bits(self.sources, self.unknown, self.boundary_touch)
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
/// | `identity: Minted(id)` | `sources ∪ {id}` |
/// | `identity: BoundaryTouch` | `boundary_touch = true` |
/// | `identity: Unmintable` | `unknown = true` |
/// | `Rooted` verdict on a command that ran | `sources ∪ verdict.sources` |
/// | `BoundaryTouch` (any) | `sources ∪ verdict.sources` |
/// | `BoundaryTouch` with `out_of_root_touch` | `boundary_touch = true`, **and** the in-root ids still cross |
/// | `BoundaryTouch` without sources (out-of-root, no other path named) | `boundary_touch = true` |
/// | `Unknown` | `unknown = true` |
/// | any verdict on a `NotRun` command | nothing |
///
/// The `out_of_root_touch` row is REQ-619's verify (C2). The table used to key the bit on
/// `sources` being empty, which is a *proxy* for "the touch was in-root" and is
/// wrong for one shape: a command that names an out-of-root boundary file and
/// an ordinary in-root one carries both, and the proxy read the ordinary file
/// as proof the touch was nameable. `Verdict::out_of_root_touch` states it
/// instead of inferring it, and the two are folded together rather than in
/// exclusive arms — an in-root id is worth keeping even when the bit is set.
///
/// `identity` is not an `Option` any more (REQ-619 verify, M6). A skill file
/// that will not mint is a file under neither the session root nor the home,
/// and the answer has always been "fail closed" — but there are two ways to
/// fail closed and they are not interchangeable. A file whose *path* a boundary
/// glob names is [`SkillIdentity::BoundaryTouch`], whose pin is permanent; one
/// nothing names is [`SkillIdentity::Unmintable`], whose pin `/shell allow`
/// lifts. Collapsed into a single `None` the stricter case was served as the
/// laxer (REQ-585 ADR-9, REQ-614 ADR-614-3).
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
pub fn fold_expansion(identity: SkillIdentity, runs: &[PreambleRun]) -> ExpansionProvenance {
    let mut folded = ExpansionProvenance::default();
    match identity {
        SkillIdentity::Minted(id) => {
            folded.sources.insert(id);
        }
        SkillIdentity::BoundaryTouch => folded.boundary_touch = true,
        SkillIdentity::Unmintable => folded.unknown = true,
    }
    for run in runs {
        if !did_spawn(&run.outcome) {
            continue;
        }
        match run.verdict.kind {
            VerdictKind::Rooted => folded.sources.extend(run.verdict.sources.iter().cloned()),
            // The in-root ids come across whatever else the command touched, so
            // egress refuses naming the actual file — a `cat .env` preamble and
            // a `read` of `.env` produce the same `privacy_block`.
            //
            // The bit is set from the verdict's own `out_of_root_touch`, never
            // from `sources.is_empty()` (REQ-619 verify, C2). The two agree for
            // a command that names one path and disagree for
            // `` !`cat ~/.ssh/id_rsa README.md` ``, where the emptiness test
            // read `{README.md}` as proof the touch was in-root and folded a
            // private key into a liftable `Sources`.
            VerdictKind::BoundaryTouch => {
                folded.sources.extend(run.verdict.sources.iter().cloned());
                folded.boundary_touch |=
                    run.verdict.out_of_root_touch || run.verdict.sources.is_empty();
            }
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
            out_of_root_touch: false,
            reason: "fixture",
        }
    }

    /// A `BoundaryTouch` whose matched path lay **outside** the session root,
    /// beside whatever in-root ids the same command named (REQ-619 verify, C2).
    ///
    /// The shape the old table could not express: `sources` non-empty *and* the
    /// touch unnameable. Reading `sources.is_empty()` as "the touch was in-root"
    /// answered `Sources({README.md})` for `` !`cat ~/.ssh/id_rsa README.md` ``.
    fn out_of_root_touch_with(sources: &[&str]) -> Verdict {
        Verdict {
            kind: VerdictKind::BoundaryTouch,
            sources: sources.iter().map(|s| fixture_id(s)).collect(),
            out_of_root_touch: true,
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
    ///
    /// The C2 row has its own (REQ-619 verify): drop `run.verdict.out_of_root_touch ||`
    /// from the `BoundaryTouch` arm, leaving the old `sources.is_empty()`
    /// reading, and **exactly one** assertion in the library goes red — this
    /// test's *an out-of-root touch is permanent however many in-root files
    /// rode along* — plus exactly one at the wire, the e2e
    /// `a_preamble_touching_a_boundary_outside_the_root_beside_an_in_root_file_is_refused`,
    /// which sees no `privacy_block` at all. Two, one per altitude, and neither
    /// existed before: every other row here names either an in-root path or no
    /// path, and the bug lives only where a command names both.
    #[test]
    fn the_fold_follows_the_adr_table() {
        let skill = fixture_id(".claude/skills/release/SKILL.md");

        // Row: `identity: Some(id)`.
        let only_identity = fold_expansion(SkillIdentity::Minted(skill.clone()), &[]);
        assert_eq!(ids(&only_identity), [".claude/skills/release/SKILL.md"]);
        assert!(!only_identity.unknown, "a minted skill file pins nothing");
        assert!(!only_identity.boundary_touch);

        // Row: `identity: None`.
        let no_identity = fold_expansion(SkillIdentity::Unmintable, &[]);
        assert!(
            no_identity.unknown,
            "a file with no identity to mint fails closed"
        );
        assert!(!no_identity.boundary_touch, "and it names no path either");
        assert!(ids(&no_identity).is_empty());

        // Row: `Rooted` on a command that ran — the AC-3 shape, and the one a
        // proportionate fold exists for.
        let rooted = fold_expansion(
            SkillIdentity::Minted(skill.clone()),
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
            SkillIdentity::Minted(skill.clone()),
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
            SkillIdentity::Minted(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], ran())],
        );
        assert!(out_of_root.boundary_touch, "the bit is the only carrier");
        assert_eq!(ids(&out_of_root), [".claude/skills/release/SKILL.md"]);

        // Row: `BoundaryTouch` **with** sources *and* `out_of_root_touch` — the
        // shape the `sources.is_empty()` proxy could not express (REQ-619
        // verify, C2). `` !`cat ~/.ssh/id_rsa README.md` `` names an in-root
        // file and touches an out-of-root boundary in one command; the fold
        // must keep the id **and** set the bit, because the two facts are about
        // two different files.
        let mixed = fold_expansion(
            SkillIdentity::Minted(skill.clone()),
            &[PreambleRun {
                verdict: out_of_root_touch_with(&["README.md"]),
                outcome: ran(),
            }],
        );
        assert!(
            mixed.boundary_touch,
            "an out-of-root touch is permanent however many in-root files rode along"
        );
        assert_eq!(
            ids(&mixed),
            [".claude/skills/release/SKILL.md", "README.md"],
            "and the in-root ids are still named"
        );

        // Row: `Unknown` — an opaque verb, liftable.
        let unknown = fold_expansion(
            SkillIdentity::Minted(skill.clone()),
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
            SkillIdentity::Minted(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], ran())],
        );
        let exit_one = fold_expansion(
            SkillIdentity::Minted(skill.clone()),
            &[run(VerdictKind::BoundaryTouch, &[], exited(1))],
        );
        let exit_two = fold_expansion(
            SkillIdentity::Minted(skill.clone()),
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
                SkillIdentity::Minted(skill),
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
            SkillIdentity::Minted(skill.clone()),
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
            fold_expansion(SkillIdentity::Minted(skill.clone()), &[]),
            "four declined commands must fold as no commands at all"
        );

        // Non-vacuity: the same four verdicts on commands that ran say all three
        // things.
        let spawned = fold_expansion(
            SkillIdentity::Minted(skill),
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
    /// The precedence itself belongs to [`ToolProvenance::from_bits`] and is
    /// tested there row for row; what this test pins is that *this* consumer
    /// still routes through it — i.e. that an expansion's three fields land in
    /// the three parameters, in that order, with nothing re-decided on the way.
    ///
    /// The middle case is REQ-619 verify C1 and is the reason the hand-written
    /// match is gone. An expansion that is `unknown` **and** names
    /// `secrets/prod.env` used to map to a bare `Unknown`: the send was refused,
    /// so nothing looked wrong — and then `/shell allow` lifted the opacity over
    /// an empty source set, which is a *clean* provenance, and the secret left.
    /// `UnknownWith` keeps the id for the glob to match after the lift.
    ///
    /// # Mutation
    ///
    /// Ran with `into_tool_provenance` reverted to the hand-written match
    /// (`unknown → ToolProvenance::Unknown`, the leak): **two red**, this test
    /// on `an unknown expansion still names what it proved` (`left: Unknown,
    /// right: UnknownWith({secrets/prod.env})`) and the e2e
    /// `a_model_invoked_skill_with_an_opaque_and_a_boundary_preamble_keeps_the_file_after_a_lift`,
    /// which is the same bug measured at the wire. Restored: green.
    ///
    /// Ran again with the `sources`/`unknown` arguments to `from_bits` swapped
    /// — which does not compile, and that is the point of routing through a
    /// typed shared mapping rather than three hand-written matches.
    #[test]
    fn the_tool_provenance_mapping_is_the_shell_tools() {
        let sources: BTreeSet<_> = [fixture_id("README.md")].into_iter().collect();
        let secret: BTreeSet<_> = [fixture_id("secrets/prod.env")].into_iter().collect();

        assert_eq!(
            ExpansionProvenance {
                sources: sources.clone(),
                unknown: false,
                boundary_touch: false,
            }
            .into_tool_provenance(),
            ToolProvenance::Sources(sources.clone())
        );
        // C1: unknown reach that nonetheless proved a path keeps the path.
        assert_eq!(
            ExpansionProvenance {
                sources: secret.clone(),
                unknown: true,
                boundary_touch: false,
            }
            .into_tool_provenance(),
            ToolProvenance::UnknownWith(secret),
            "an unknown expansion still names what it proved"
        );
        // And an unknown expansion with nothing proved is still the bare
        // sentinel — the arm above must not have swallowed this one.
        assert_eq!(
            ExpansionProvenance {
                sources: BTreeSet::new(),
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
