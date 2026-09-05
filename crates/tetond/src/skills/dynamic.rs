//! Dynamic context: the `` !`cmd` `` scanner, the typed outcome, and the one
//! I/O edge that turns commands into outcomes (REQ-585 BR-6, BR-14).
//!
//! # The grammar, in full
//!
//! A dynamic-context slot is `` !`…` ``: a `!` immediately followed by a
//! backtick, then every byte up to the **next** backtick. That is the whole
//! grammar. In particular:
//!
//! - **No nesting.** The first backtick after the opener closes the run; a
//!   backtick inside a command cannot be quoted into the command text.
//! - **No escape.** There is no way to write a literal `` !` `` that the
//!   scanner will not take, and none is invented here: BR-13 says the body is
//!   passed as written, and an escape syntax is a rewrite of the body under a
//!   different name.
//! - **An unterminated run is not a command.** `` !`ls `` with no closing
//!   backtick is literal body text and reaches the model as the author typed
//!   it. Nothing is run, nothing is asked for, and no diagnostic is minted —
//!   a body is prose, and prose containing a stray backtick is not an error.
//! - **An empty run is not a command.** `` !`` `` (or one holding only
//!   whitespace) has nothing to run, so it stays literal rather than becoming
//!   a slot that would put an empty line in a consent prompt.
//!
//! # Where the split falls
//!
//! [`scan`] is pure — it is the half BR-14 requires to be unit-testable with
//! no daemon and no terminal — and [`run_all`] is the single I/O edge, one
//! [`run_bounded`] call per command, in document order. Nothing here calls
//! `Tool::refine`: that fires the `shell` duty, which is a model call, and
//! BR-4 forbids a model call at expansion time.
//!
//! # The verdict is taken here, before the spawn (REQ-619 BR-1)
//!
//! Each command is handed to [`classify`] — REQ-614's reach grammar, reached
//! through [`crate::harness::tools`] so this module names one grammar rather
//! than a copy of it — **before** [`run_bounded`] is called for it, and the
//! pair travels out as one [`PreambleRun`]. Two properties fall out of the
//! shape rather than out of a rule someone has to keep:
//!
//! - **Output and exit status cannot reach the verdict** (BR-2), because
//!   [`classify`] takes neither by signature and the call happens before there
//!   is anything to take.
//! - **The two callers cannot disagree** (BR-1, REQ-614 BR-10). The typed
//!   `/name` path and the model's `skill` tool used to each fold `spawned()`
//!   their own way; the verdict is computed by the same loop that spawns, so
//!   there is only one answer to fold.
//!
//! A command the door leaves unrun is still classified — the classifier reads
//! command text, never a process — and still not spawned; the fold
//! (ADR-619-4) is where "a command that did not run contributes nothing" is
//! decided, not here.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use teton_core::entities::PrivacyBoundary;
use teton_protocol::events::{DynamicOutcomeView, NotRunReason};

use teton_protocol::methods::{RefusalReason, RootKind};

use crate::harness::permissions::SkillConsent;
use crate::harness::tools::shell::{cap_output, run_bounded, BoundedRun};
use crate::harness::tools::{classify, Verdict, VerdictKind};

/// The wall-clock budget for **one invocation's whole** dynamic-context run
/// (BUG-185).
///
/// The per-command timeout bounds a single command; nothing bounded their sum.
/// `run_all` runs every slot sequentially inside one `spawn_blocking`, and that
/// work is **not cancellable** — connection teardown aborts the `await` and
/// leaves the closure running, so the session stays claimed and the daemon
/// stays awake. A ceiling on the total is the only thing that ends it.
///
/// Two minutes: far above any real skill (the shipped bodies run `git` and
/// `ls`) and far below the 16 minutes [`crate::skills::MAX_DYNAMIC_COMMANDS`]
/// slots could otherwise reach at the default per-command timeout.
pub const INVOCATION_BUDGET_MS: u64 = 120_000;

/// The opener: a `!` immediately followed by a backtick.
const OPEN: &str = "!`";

/// One `` !`cmd` `` occurrence's command text, verbatim **after**
/// `$ARGUMENTS`/`$N` substitution (BR-4 precedes BR-6).
///
/// A newtype rather than a bare `String` because these strings are handed to a
/// shell and listed in a consent prompt; a function that takes `&[Command]`
/// cannot be passed a list of anything else by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    text: String,
}

impl Command {
    /// The command a slot holds.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The command text, exactly as it will be run and as the consent prompt
    /// lists it.
    ///
    /// **Verbatim, and deliberately so.** Every surface that *renders* this
    /// string — the fold's not-run placeholder in particular — bounds and
    /// defuses it there, at the frame it is being spliced into (ADR-10). Doing
    /// it here instead would change the command that runs.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// One span of a scanned body: literal text, or the *n*th command's slot.
///
/// `at_line_start` records whether the chunk began at a line start **in the
/// body** — offset 0, or just past a `\n`. The expander needs it because
/// `neutralize_envelope_tags` treats offset 0 of whatever it is handed as a
/// line start, and a chunk that begins mid-line (right after a
/// `` !`cmd` `` that had text before it on the same line) would otherwise be
/// defused at a position that is not a line start at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Body text to splice as-is.
    Text {
        /// The chunk.
        text: String,
        /// Whether the chunk starts at a line start in the body.
        at_line_start: bool,
        /// The chunk's byte offset in the scanned body.
        ///
        /// Carried so the expander can tell **which bytes in this chunk came
        /// from the caller** (BUG-190). `substitute` records those spans in
        /// body coordinates; without an origin here they cannot be mapped onto
        /// a chunk, and file prose and argument text stay indistinguishable in
        /// the region the frame vouches for.
        from: usize,
    },
    /// The slot of the command at this index in the scan's command list.
    Slot(usize),
}

/// Split `body` into literal pieces and the ordered command list.
///
/// Pure. Commands come out in **document order**, and the slot indices are
/// positions in that list — so the fold cannot put one command's output in
/// another's place without the index disagreeing.
pub(crate) fn scan(body: &str) -> (Vec<Piece>, Vec<Command>) {
    let mut pieces = Vec::new();
    let mut commands: Vec<Command> = Vec::new();
    // The literal chunk that has not been pushed yet starts here …
    let mut chunk_start = 0usize;
    // … and the next opener is looked for from here. The two differ only while
    // a run is being skipped (an empty command), where the bytes stay literal.
    let mut cursor = 0usize;

    while let Some(rel) = body[cursor..].find(OPEN) {
        let opener = cursor + rel;
        let text_start = opener + OPEN.len();
        let Some(close_rel) = body[text_start..].find('`') else {
            // Unterminated: the rest of the body is literal text.
            break;
        };
        let close = text_start + close_rel;
        let text = &body[text_start..close];
        if text.trim().is_empty() {
            // Not a command. Leave the bytes in the literal chunk and resume
            // after the opening backtick, so a later opener still scans.
            cursor = text_start;
            continue;
        }
        push_text(&mut pieces, body, chunk_start, opener);
        pieces.push(Piece::Slot(commands.len()));
        commands.push(Command::new(text));
        chunk_start = close + 1;
        cursor = close + 1;
    }
    push_text(&mut pieces, body, chunk_start, body.len());
    (pieces, commands)
}

/// Push `body[from..to]` as a literal piece, unless it is empty.
fn push_text(pieces: &mut Vec<Piece>, body: &str, from: usize, to: usize) {
    if from >= to {
        return;
    }
    pieces.push(Piece::Text {
        text: body[from..to].to_owned(),
        at_line_start: from == 0 || body.as_bytes()[from - 1] == b'\n',
        from,
    });
}

/// What one dynamic command came to.
///
/// Typed, never prose a renderer parses (BR-6). The four arms are the four
/// things the fold has to say differently: output to inline, a command that
/// **never ran** and why, one that ran and **failed**, and one killed on the
/// deadline. A command's failure never fails the invocation — every arm
/// produces a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicOutcome {
    /// The command ran, exited zero, and this is its stdout — trimmed of
    /// trailing whitespace and capped by [`cap_output`].
    Ran {
        /// The captured stdout.
        output: String,
        /// Whether this output came from a `||` **fallback** branch because
        /// the primary exited non-zero (REQ-615 BR-6).
        ///
        /// Observable only because [`run_one`] splits the command at its first
        /// top-level `||` and runs the primary itself: handing
        /// `cat X || echo none` to one shell returns exit 0 and the fallback's
        /// stdout, byte-identical to a primary that succeeded.
        ///
        /// No `Default`, deliberately — every construction site states it, so
        /// "the fold knows whether this is the project's answer or a stand-in
        /// for it" is a compile-time property rather than a field someone
        /// forgets (architecture.md: a required field with no `Default`).
        fell_back: bool,
        /// Whether the `shell` tool's ceiling threw information away, so the
        /// model is reading a **prefix**.
        ///
        /// Recorded here, from [`cap_output`]'s own answer, rather than
        /// re-derived by a reader: the ceiling lives in exactly one module and
        /// a caller that compared against it would be a second copy of it
        /// (`the_output_cap_has_exactly_one_home`). It is also not recoverable
        /// from `output` alone — a capped body's length and the length it was
        /// cut from are independent numbers.
        truncated: bool,
    },
    /// The command was **not run**, for a reason that is not the command's
    /// own: the permission level, the answer, the missing terminal, or a
    /// failure to start at all. `reason` is the `<reason>` half of the
    /// placeholder.
    NotRun {
        /// Why it did not run.
        reason: String,
    },
    /// The command ran and did not exit zero. `status` is the rendered exit
    /// (`exited 1`); the output is **not** carried — BR-6 says a failed
    /// command leaves a placeholder in its place.
    Failed {
        /// The rendered exit status.
        status: String,
        /// The exit code behind `status`, or `None` when a signal killed the
        /// process (or the run was lost) and there is no code to report.
        ///
        /// Carried **beside** the rendered form rather than recovered from it.
        /// [`crate::runtime`] reports this number on the wire
        /// (`skill_invoked`'s `exit_status`), and a reader that re-read it out
        /// of `status` would be a second parser of this module's own sentence
        /// — the shape LESSON-529 names, one layer down.
        exit_status: Option<i32>,
    },
    /// The deadline passed and the process group was killed.
    TimedOut,
}

impl DynamicOutcome {
    /// The outcome for a command the user declined (AC-8's wording, which is
    /// pinned by that AC's placeholder text).
    #[must_use]
    pub fn declined() -> Self {
        Self::NotRun {
            reason: "declined".to_owned(),
        }
    }

    /// The outcome at the `plan` permission level, where no command runs and
    /// the placeholder names the level (AC-9).
    #[must_use]
    pub fn not_run_at_plan() -> Self {
        Self::NotRun {
            reason: "plan permission level".to_owned(),
        }
    }

    /// The outcome on a pipe at a level that would ask: the client refuses
    /// without reading stdin, so nobody was asked (BR-11, AC-9).
    ///
    /// Distinct from [`Self::declined`] on purpose — "you said no" and "there
    /// was nobody to ask" are different facts about the same missing output.
    ///
    /// It is also the outcome when the question could not be **put** to anyone
    /// at all — no addressed-delivery route, a connection that would not take
    /// the frame, or a client that went away before answering
    /// ([`SkillConsent::Unanswerable`](crate::harness::permissions::SkillConsent::Unanswerable)).
    /// Those differ in *why* there was nobody at the other end, and not in the
    /// sentence the model is owed: no human could be asked.
    #[must_use]
    pub fn no_terminal() -> Self {
        Self::NotRun {
            reason: "no terminal, so no human could be asked".to_owned(),
        }
    }

    /// The outcome when the client refused because it did not recognize the
    /// request's subject (ADR-7's fail-closed rule).
    ///
    /// A fourth sentence rather than a reuse of [`Self::no_terminal`], because
    /// this client had a terminal and chose not to guess: telling the model
    /// there was no terminal would name the wrong remedy (upgrade the client,
    /// not find a human).
    #[must_use]
    pub fn unrecognized_subject() -> Self {
        Self::NotRun {
            reason: "the client did not recognize the request, so nobody was asked".to_owned(),
        }
    }

    /// The outcome when consent was **given** and the command still never
    /// started — the shell was missing, the jail root would not resolve, the
    /// spawn failed.
    ///
    /// Not a closed door, and deliberately worded so it cannot be mistaken for
    /// one: every other `NotRun` sentence answers "who said no", and this one
    /// says the answer was yes and the machine could not carry it out. Calling
    /// it a failure instead would tell the model it was attempted and exited,
    /// which points a reader at the wrong fix.
    #[must_use]
    pub fn could_not_start() -> Self {
        Self::NotRun {
            reason: "it could not be started on this machine".to_owned(),
        }
    }

    /// The invocation's whole-run budget was already spent (BUG-185).
    ///
    /// A **not-run**, not a [`Self::TimedOut`]: this command was never started,
    /// and reporting it as timed out would tell the reader it ran and was
    /// killed, which points at the wrong command to fix. The one that overran
    /// is the one before it.
    #[must_use]
    pub fn budget_exhausted() -> Self {
        Self::NotRun {
            reason: format!(
                "this skill's dynamic context passed its {}s total budget before this command started",
                INVOCATION_BUDGET_MS / 1000
            ),
        }
    }

    /// True when this outcome carries output to inline.
    ///
    /// **Not the same question as [`Self::spawned`]**, and the difference is
    /// load-bearing in both directions: this one decides whether there is text
    /// to splice, that one decides whether the turn can still be pinned. A
    /// failing command has no output and still ran.
    #[must_use]
    pub fn did_run(&self) -> bool {
        matches!(self, Self::Ran { .. })
    }

    /// True when a process was actually started — output or not.
    ///
    /// This is the provenance question, and it is deliberately wider than
    /// [`Self::did_run`]. `ShellTool::run` tags **every** arm that spawned
    /// something with `.with_unknown_provenance()` — success, non-zero exit,
    /// timeout and lost alike — because a shell command runs arbitrary code and
    /// the daemon cannot know which files its result was derived from. A
    /// *result* is not only its stdout: an exit status is a value the command
    /// chose.
    ///
    /// **Its provenance use is retired by REQ-619.** BR-1/BR-2 replace "any
    /// command spawned ⇒ the expansion is unknown" with the per-command
    /// [`Verdict`] [`run_all`] now takes, which proves what it can and
    /// fail-closes on the rest; the fold reads [`PreambleRun::verdict`], not
    /// this. It stays because a *renderer* still asks whether a process was
    /// started, and because deleting it would rewrite tests that are about
    /// something else — but a new provenance decision that reads it is a
    /// second answer to a question that now has one home (TASK-401 flips the
    /// two callers).
    ///
    /// Asking `did_run` here instead opened a side channel that pinned nothing.
    /// A body of `` !`grep -q AKIA secrets/prod.env && exit 1 || exit 2` ``
    /// produces no output, so the turn stayed pinnable, while the placeholder
    /// carried `exited 1` / `exited 2` — one bit per command about a
    /// `local-only` file — into a turn that then routed remote (REQ-585
    /// verify). `TimedOut` is the same channel with a sleep.
    #[must_use]
    pub fn spawned(&self) -> bool {
        match self {
            Self::Ran { .. } | Self::Failed { .. } | Self::TimedOut => true,
            Self::NotRun { .. } => false,
        }
    }

    /// Whether the `shell` tool's ceiling threw information away — i.e. the
    /// model is reading a **prefix** of what the command printed.
    ///
    /// A read of what [`cap_output`] already answered ([`Self::Ran`]'s
    /// `truncated`), never a re-derivation: the ceiling has one home and a
    /// reader that compared against it would be a second one.
    #[must_use]
    pub fn output_truncated(&self) -> bool {
        matches!(
            self,
            Self::Ran {
                truncated: true,
                fell_back: false,
                ..
            }
        )
    }

    /// The `<reason>` half of the not-run placeholder, or `None` for the arm
    /// that has output instead.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ran { .. } => None,
            Self::NotRun { reason } => Some(reason),
            Self::Failed { status, .. } => Some(status),
            Self::TimedOut => Some("timed out"),
        }
    }
}

/// The gate's answer to one invocation, reduced to the fact every reader needs:
/// **which door was closed**, or `None` when the commands may run.
///
/// Two arms of [`SkillConsent`] collapse here and it is deliberate.
/// `Refused(NoTerminal)` and `Unanswerable` differ in *why* there was nobody at
/// the other end — a client that refused without reading stdin, versus a
/// question that could not be put to anyone at all — and not in anything the
/// model or the user can act on: no human was asked. Every other arm keeps its
/// own door, because REQ-585 AC-9 turns on the difference between "you said no"
/// and "nobody could be asked", and BR-6 on the difference between either and
/// "the level does not run them".
///
/// **Here rather than at one caller** (REQ-587). The user-typed `/name` path
/// (`runtime::DaemonRuntime::settle_dynamic_context`) and the model's `skill` tool
/// ([`crate::harness::tools::skill`]) ask the same gate the same question and
/// must read the same answer out of it; a second copy of this match is how the
/// two callers come to disagree about what a decline is (LESSON-528). It lives
/// beside the placeholder constructors it feeds for the same reason those live
/// here: one home for the four sentences a model can read about a command that
/// did not run.
#[must_use]
pub fn closed_door(consent: SkillConsent) -> Option<NotRunReason> {
    match consent {
        SkillConsent::Allowed => None,
        SkillConsent::DeniedByLevel => Some(NotRunReason::Level),
        SkillConsent::Declined => Some(NotRunReason::Declined),
        SkillConsent::Refused(RefusalReason::NoTerminal) | SkillConsent::Unanswerable => {
            Some(NotRunReason::NoTerminal)
        }
        SkillConsent::Refused(RefusalReason::UnrecognizedSubject) => {
            Some(NotRunReason::UnrecognizedSubject)
        }
    }
}

/// The placeholder sentence a closed door earns, from this module's own
/// constructors.
#[must_use]
pub fn door_outcome(door: NotRunReason) -> DynamicOutcome {
    match door {
        NotRunReason::Declined => DynamicOutcome::declined(),
        NotRunReason::Level => DynamicOutcome::not_run_at_plan(),
        NotRunReason::NoTerminal => DynamicOutcome::no_terminal(),
        NotRunReason::UnrecognizedSubject => DynamicOutcome::unrecognized_subject(),
        // Never reached from `closed_door`, which only ever answers with the
        // consent's own doors. It is spelled out rather than left to an
        // `unreachable!` because `NotRunReason` now also carries the runner's
        // `CouldNotStart`, which arrives as an *outcome* and not as a door —
        // and a future caller should meet a sentence here, not a panic.
        NotRunReason::CouldNotStart => DynamicOutcome::could_not_start(),
        // Unreachable here for a structural reason, not a contingent one:
        // `Unknown` exists so an *older client* can parse a newer daemon's
        // frame (BUG-186), and the daemon is the producer. `not_run` keeps the
        // arm total without inventing a door that was never closed.
        NotRunReason::Unknown => DynamicOutcome::NotRun {
            reason: "it did not run".to_owned(),
        },
    }
}

/// One command's typed record for BR-12's event, projected from the outcome the
/// daemon actually produced (LESSON-544: the wire value is derived from the real
/// one, never composed beside it).
///
/// `door` is `Some` when the consent closed one, and it is what a not-run arm
/// reports: the daemon-side outcome carries its reason as prose for the model,
/// and re-reading a [`NotRunReason`] out of that sentence would be a second
/// parser of the daemon's own words (LESSON-529).
///
/// A command the runner could not **start** (`sh` unavailable, an unresolvable
/// jail root) has no door either, and reports [`NotRunReason::CouldNotStart`].
/// It is not folded into `Failed`: that would say it was attempted and exited,
/// which is untrue and points a reader at the wrong fix. It is not folded into
/// one of the consent's doors either — nobody refused it.
/// The run — verdict and outcome — is what this projection is given, because
/// the verdict's kind and its content-free reason ride the wire beside the
/// `ran` / `failed` / `timed out` facts (ADR-619-5, BR-7).
///
/// **Only the kind and the reason cross.** `Verdict::sources` stays here: it is
/// the fold's input (ADR-619-4), not a surface's, and ids on a `/verbose` line
/// would be a second, unasked-for disclosure of what the command read. The
/// reason is `&'static str`, so what reaches the event cannot be assembled from
/// the command or its output — BR-7 holds by the type, not by care here.
///
/// The wire [`teton_protocol::events::Reach`] is spelled in full because it is
/// a *different* thing from this module's [`PreambleReach`]: the wire enum is
/// one command's answer, three values wide, and `PreambleReach` is the
/// classifier's four-value *input*. They used to share the word `PreambleReach`, which
/// is why every mention of either had to be qualified to be read (REQ-619
/// verify, m5); the same collision still governs [`DynamicOutcome`] below,
/// where the two names are genuinely two views of one fact.
#[must_use]
pub fn outcome_view(
    command: &Command,
    run: &PreambleRun,
    door: Option<NotRunReason>,
) -> DynamicOutcomeView {
    DynamicOutcomeView {
        reach: Some(match run.verdict.kind {
            VerdictKind::Rooted => teton_protocol::events::Reach::Rooted,
            VerdictKind::BoundaryTouch => teton_protocol::events::Reach::BoundaryTouch,
            VerdictKind::Unknown => teton_protocol::events::Reach::Unknown,
        }),
        reach_reason: Some(run.verdict.reason.to_owned()),
        // File-supplied bytes on a surface: bounded and rendered on one line
        // here, at the same ceiling the fold's echoed placeholder uses (BR-3).
        command: teton_core::session_root::bounded_field(
            command.as_str(),
            crate::skills::expand::COMMAND_ECHO_MAX_CHARS,
        ),
        outcome: match &run.outcome {
            DynamicOutcome::Ran { output, .. } => teton_protocol::events::DynamicOutcome::Ran {
                output_bytes: output.len() as u64,
                truncated: run.outcome.output_truncated(),
            },
            DynamicOutcome::NotRun { .. } => teton_protocol::events::DynamicOutcome::NotRun {
                reason: door.unwrap_or(NotRunReason::CouldNotStart),
            },
            DynamicOutcome::Failed { exit_status, .. } => {
                teton_protocol::events::DynamicOutcome::Failed {
                    exit_status: *exit_status,
                }
            }
            DynamicOutcome::TimedOut => teton_protocol::events::DynamicOutcome::TimedOut,
        },
    }
}

/// How far a preamble command is allowed to reach — the four values
/// `ShellTool::run` hands [`classify`], carried as one input so the runner and
/// the `shell` tool cannot be given different ones (REQ-619 ADR-619-1).
///
/// Named `PreambleReach` and not `PreambleReach` because
/// [`teton_protocol::events::Reach`] is a live, unrelated type in the same
/// sentences — one command's *answer* on the wire, against this type's
/// four-value *input* to the classifier (REQ-619 verify, m5). Two things one
/// word apart, in a module that mentions both.
///
/// **Built from the turn's `ToolContext`, never from `config`.** The jail
/// applies `ctx.denied_prefixes()`; a `PreambleReach` assembled beside it from the
/// configuration would classify against a different denial set than the one
/// the command actually runs under, and the two would agree only until one of
/// them was edited. `root` is the session root the commands already run with
/// as cwd (REQ-585 BR-6), so the classifier resolves the same relative paths
/// the shell will.
#[derive(Debug, Clone)]
pub struct PreambleReach {
    /// The session root: the commands' cwd, and the root every path argument
    /// is resolved against.
    pub root: PathBuf,
    /// What kind of place that root is. `Rooted` is reachable only from a
    /// `Project` root (ADR-614-2).
    pub root_kind: RootKind,
    /// The session's privacy boundaries, builtin rows included. Empty means
    /// "nothing to protect", and the classifier answers `Unknown` from its
    /// first line — the pre-REQ-614 answer, which BR-9 keeps.
    pub boundaries: Vec<PrivacyBoundary>,
    /// The prefixes the jail denies (the transcript directory, and whatever
    /// else the context denied), so a path under one is never called rooted.
    pub denied_prefixes: Vec<PathBuf>,
}

/// One preamble command's verdict and what it came to.
///
/// The two halves travel together because the fold needs both and because
/// separating them is how a caller ends up zipping two lists that drifted
/// (ADR-619-4 reads this; TASK-400 writes that fold). The verdict is present
/// for a command that never ran too — a closed door and an exhausted budget
/// both leave a `NotRun` outcome beside a real classification, and it is the
/// **fold** that decides such a command contributes nothing (BR-2), not the
/// runner.
#[derive(Debug, Clone)]
pub struct PreambleRun {
    /// What the daemon could prove about this command's reach, taken before
    /// the command spawned.
    pub verdict: Verdict,
    /// What the command came to.
    pub outcome: DynamicOutcome,
}

/// One preamble command's verdict, from REQ-614's grammar.
///
/// The one place the `skills` module calls [`classify`], so the arguments are
/// assembled once: [`run_all`] uses it per command, and a caller holding a
/// command the **consent door** left unrun uses it to classify what it did not
/// run (BR-1 — the verdict is a fact about the command text, not about a
/// process).
#[must_use]
pub fn preamble_verdict(reach: &PreambleReach, command: &Command) -> Verdict {
    classify(
        &reach.root,
        reach.root_kind,
        &reach.boundaries,
        reach.denied_prefixes.clone(),
        command.as_str(),
    )
}

/// Run every command in order under the `shell` tool's jail, composed
/// environment, `PATH` floor, process group and deadline — the single I/O edge
/// of this feature (BR-6, BR-14).
///
/// The environment is an allowlist, not a scrub, since REQ-596: these commands
/// are as model-driven as a `shell` call and inherit the same policy, because
/// they go through the same `run_bounded`.
///
/// Sequential and in document order, because that is the order the user
/// consented to and the order the body reads in. `timeout_ms` is per command,
/// as it is for the `shell` tool.
///
/// The gate is **not** here: this function runs what it is given. Deciding
/// whether it may run at all is the caller's, under the skill's own permission
/// key (ADR-6), and a caller that decided *no* builds the [`DynamicOutcome`]s
/// itself rather than calling this — classifying them with
/// [`preamble_verdict`], because a command that did not run still has a reach.
///
/// **Each command is classified before it spawns** (REQ-619 BR-1). One
/// [`classify`] call per command, taken at the top of that command's turn
/// through the loop and reused by every arm below it, so an arm cannot reach a
/// different answer and neither output nor exit status can reach any answer at
/// all (BR-2).
#[must_use]
pub fn run_all(reach: &PreambleReach, commands: &[Command], timeout_ms: u64) -> Vec<PreambleRun> {
    run_all_within(reach, commands, timeout_ms, INVOCATION_BUDGET_MS)
}

/// [`run_all`] against an explicit whole-invocation budget.
///
/// Split out so the budget is reachable from a test without one waiting
/// [`INVOCATION_BUDGET_MS`]. Production has exactly one budget and
/// [`run_all`] is the only caller that supplies it, so no call site can pick a
/// different ceiling by accident.
pub(crate) fn run_all_within(
    reach: &PreambleReach,
    commands: &[Command],
    timeout_ms: u64,
    budget_ms: u64,
) -> Vec<PreambleRun> {
    run_all_with(reach, commands, timeout_ms, budget_ms, &preamble_verdict)
}

/// [`run_all_within`] with the classifier exposed.
///
/// The seam a test needs to *count* the verdicts and to see where each one
/// falls relative to its command's spawn: the real classifier reports the same
/// answer however many times it is asked, so a test built on it alone could
/// not tell "once per command, first" from "twice per command, last"
/// (LESSON-569 — verify the failure mechanism, do not reason about it). The
/// budget is a parameter here for the same reason it is one in
/// `shell_provenance::classify_with_budget`.
fn run_all_with(
    reach: &PreambleReach,
    commands: &[Command],
    timeout_ms: u64,
    budget_ms: u64,
    classify_with: &dyn Fn(&PreambleReach, &Command) -> Verdict,
) -> Vec<PreambleRun> {
    let started = Instant::now();
    let budget = Duration::from_millis(budget_ms);
    commands
        .iter()
        .map(|command| {
            // BUG-185's second half, and **first** (REQ-619 verify, m3). The
            // slot cap bounds the *count*; this bounds the *total*, and both
            // are needed: 32 slots each taking the full per-command timeout is
            // still 16 minutes on a blocking-pool thread that `spawn_blocking`
            // cannot cancel.
            //
            // The classification used to sit above this check, which made the
            // budget's own arithmetic wrong in the direction it exists to
            // prevent: `classify` may walk a subtree for up to 1.5 s per
            // command (`shell_provenance::classify_with_budget`), so a
            // 32-command invocation could spend 48 s *classifying* commands the
            // budget had already stopped, and spend it after the deadline it
            // was measured against. Deciding not to run something is not a
            // reason to do work about it.
            let Some(left) = budget.checked_sub(started.elapsed()) else {
                // Not classified and not spawned — and the verdict says so
                // rather than being absent, because `PreambleRun` carries one
                // for every command and a half-record is worse than a
                // conservative one. `Verdict::not_classified` is content-free
                // and `Unknown`, which is the only honest answer about a
                // command nothing looked at; the fold ignores a `NotRun`'s
                // verdict anyway (BR-2), so nothing downstream can tell this
                // from the classification it replaces.
                return PreambleRun {
                    verdict: Verdict::not_classified(),
                    outcome: DynamicOutcome::budget_exhausted(),
                };
            };
            // REQ-619 BR-1/BR-2: before `run_one`, before anything that could
            // observe what the command did. A verdict taken after the spawn
            // would be the same value today and a different one the first time
            // somebody reads a file the command wrote.
            let verdict = classify_with(reach, command);
            // The per-command timeout still applies; the invocation's remaining
            // budget only ever shortens it. A command started with 2 s left is
            // killed at 2 s rather than being allowed its own 30 s and taking
            // the whole invocation past the deadline it was checked against.
            let allowed = timeout_ms.min(u64::try_from(left.as_millis()).unwrap_or(u64::MAX));
            PreambleRun {
                verdict,
                outcome: run_one(&reach.root, command, allowed),
            }
        })
        .collect()
}

/// One command's trip through [`run_bounded`], typed for the fold.
fn run_one(root: &Path, command: &Command, timeout_ms: u64) -> DynamicOutcome {
    // REQ-615 BR-6. A command with a top-level `||` is run in two steps — the
    // primary, then the fallback only if the primary failed — because that is
    // the *only* way the daemon can tell which branch produced the output. One
    // shell returns exit 0 either way.
    //
    // `a || b || c` splits into primary `a` and remainder `b || c`: only the
    // first branch's exit is observed, and the remainder goes to the shell
    // whole, so a chain's semantics stay the shell's. A command with no
    // top-level `||` skips all of this and runs exactly as it did before.
    if let Some((primary, fallback)) =
        crate::harness::root_gate::split_top_level_or(command.as_str())
    {
        // **The two steps share one command's deadline.** Splitting turned one
        // `run_bounded` into two, and handing each the full `timeout_ms` would
        // let a single `!cmd` slot take twice the per-command budget — the
        // arithmetic BUG-185 bounded, quietly reopened by a change that is not
        // about timeouts at all. What the caller allotted is what the slot
        // gets, however many shells the daemon runs inside it.
        let started = Instant::now();
        let first = run_step(root, primary, timeout_ms, false);
        if !matches!(first, DynamicOutcome::Failed { .. }) {
            return first;
        }
        let spent = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let Some(left) = timeout_ms.checked_sub(spent) else {
            // The primary consumed the whole allowance. Reporting the *timeout*
            // is honest: the slot's deadline passed, and it is the same word a
            // one-shell run would have used.
            return DynamicOutcome::TimedOut;
        };
        return run_step(root, fallback, left, true);
    }
    run_step(root, command.as_str(), timeout_ms, false)
}

/// One command string's trip through [`run_bounded`], typed for the fold.
///
/// `fell_back` is the caller's fact, not this function's: it says which side of
/// a `||` this string came from, and only [`run_one`] knows that.
fn run_step(root: &Path, command: &str, timeout_ms: u64, fell_back: bool) -> DynamicOutcome {
    match run_bounded(root, command, timeout_ms) {
        BoundedRun::Completed { status, stdout, .. } if status.success() => {
            // stdout only. BR-6 says stdout enters the expansion; a command
            // whose diagnostics matter can redirect them itself (`2>&1`), and
            // inlining stderr would put a linter's warnings in a prompt the
            // author asked for a file listing in.
            let text = String::from_utf8_lossy(&stdout).trim_end().to_owned();
            let (output, _raw_chars, truncated) = cap_output(text);
            DynamicOutcome::Ran {
                output,
                truncated,
                fell_back,
            }
        }
        BoundedRun::Completed { status, .. } => DynamicOutcome::Failed {
            status: describe_status(status),
            exit_status: status.code(),
        },
        BoundedRun::TimedOut => DynamicOutcome::TimedOut,
        // Never started: the jail root could not be resolved, or `sh` could
        // not be launched.
        BoundedRun::SpawnFailed(reason) => DynamicOutcome::NotRun { reason },
        // Started, and its output never arrived. It *ran*, so it is not a
        // not-run: the machine did something and we cannot say what.
        BoundedRun::Lost(reason) => DynamicOutcome::Failed {
            status: reason,
            exit_status: None,
        },
    }
}

/// How an exit reads in a placeholder.
fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited {code}"),
        None => "killed by a signal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-skill-dynamic-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The reach a repo-rooted session on a stock machine has: a project root
    /// and REQ-597's thirteen builtin globs, which is the configuration every
    /// claim in REQ-619 is about.
    ///
    /// `root_kind` is passed rather than probed — `classify` takes it as an
    /// argument — but the fixtures below still plant a `Cargo.toml`, because a
    /// root whose kind is asserted and whose contents disagree is a fixture
    /// that lies to the next reader.
    fn test_reach(root: &Path) -> PreambleReach {
        PreambleReach {
            root: root.to_path_buf(),
            root_kind: RootKind::Project,
            boundaries: teton_core::config::DEFAULT_BOUNDARIES
                .iter()
                .map(|glob| PrivacyBoundary::builtin(*glob))
                .collect(),
            denied_prefixes: Vec::new(),
        }
    }

    /// A project root: a temp directory with a build manifest in it.
    fn project_root(tag: &str) -> PathBuf {
        let dir = temp_root(tag);
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        dir
    }

    /// [`run_all`] for the tests that are about the **runner** and not about
    /// the verdict.
    ///
    /// They ran against the pre-REQ-619 signature and their claims are
    /// unchanged; routing them through a real [`PreambleReach`] is what keeps them
    /// exercising the production path now that TASK-401 has deleted the
    /// verdict-free shim they were briefly routed through.
    fn run(root: &Path, commands: &[Command], timeout_ms: u64) -> Vec<DynamicOutcome> {
        run_all(&test_reach(root), commands, timeout_ms)
            .into_iter()
            .map(|preamble| preamble.outcome)
            .collect()
    }

    /// [`run`] against an explicit whole-invocation budget.
    fn run_within(
        root: &Path,
        commands: &[Command],
        timeout_ms: u64,
        budget_ms: u64,
    ) -> Vec<DynamicOutcome> {
        run_all_within(&test_reach(root), commands, timeout_ms, budget_ms)
            .into_iter()
            .map(|preamble| preamble.outcome)
            .collect()
    }

    fn texts(pieces: &[Piece]) -> Vec<String> {
        pieces
            .iter()
            .map(|piece| match piece {
                Piece::Text { text, .. } => text.clone(),
                Piece::Slot(index) => format!("<slot {index}>"),
            })
            .collect()
    }

    /// The scanner's whole grammar, in one table: document order, the pieces
    /// either side of each slot, and the two runs that are **not** commands.
    #[test]
    fn the_scanner_takes_document_order_and_leaves_a_non_command_literal() {
        let (pieces, commands) = scan("a !`one` b !`two` c");
        assert_eq!(
            commands,
            vec![Command::new("one"), Command::new("two")],
            "document order, verbatim"
        );
        assert_eq!(
            texts(&pieces),
            vec!["a ", "<slot 0>", " b ", "<slot 1>", " c"]
        );

        // Unterminated: not a command, and the bytes survive.
        let (pieces, commands) = scan("before !`ls -la after");
        assert!(commands.is_empty(), "an unterminated run is not a command");
        assert_eq!(texts(&pieces), vec!["before !`ls -la after"]);

        // Empty and whitespace-only runs: not commands either, and a later
        // opener still scans.
        let (pieces, commands) = scan("x !`` y !`   ` z !`ls` w");
        assert_eq!(commands, vec![Command::new("ls")]);
        assert_eq!(
            texts(&pieces),
            vec!["x !`` y !`   ` z ", "<slot 0>", " w"],
            "the non-command bytes stay literal"
        );
    }

    /// No nesting and no escape: the first backtick closes the run, and a
    /// backslash is a byte like any other (BR-13 — the body is passed as
    /// written, so there is no escape syntax to honour).
    #[test]
    fn the_first_backtick_closes_the_run_and_nothing_escapes_it() {
        let (_, commands) = scan("!`echo `nested`` tail");
        assert_eq!(
            commands,
            vec![Command::new("echo ")],
            "the first backtick closes; what follows is body text"
        );

        let (_, commands) = scan(r"!`echo \` still` tail");
        assert_eq!(
            commands,
            vec![Command::new(r"echo \")],
            "a backslash escapes nothing"
        );
    }

    /// A chunk knows whether it began at a line start, which is the fact the
    /// expander's envelope defusing depends on (ADR-10).
    #[test]
    fn a_chunk_records_whether_it_began_at_a_line_start() {
        let (pieces, _) = scan("head !`one`tail\nnext !`two`\nlast");
        let flags: Vec<bool> = pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Text { at_line_start, .. } => Some(*at_line_start),
                Piece::Slot(_) => None,
            })
            .collect();
        assert_eq!(
            flags,
            vec![true, false, false],
            "offset 0 is a line start; text resuming after a slot is not"
        );

        let (pieces, _) = scan("!`one`\nx");
        let flags: Vec<bool> = pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Text { at_line_start, .. } => Some(*at_line_start),
                Piece::Slot(_) => None,
            })
            .collect();
        assert_eq!(
            flags,
            vec![false],
            "the chunk opens with the newline itself"
        );
    }

    /// **BR-6.** The commands run *sequentially, in document order* — asserted
    /// through a side effect, because that is the only way the claim is
    /// falsifiable: a runner that executes them backwards and then reorders the
    /// outcomes to match produces an identical list, and an assertion on the
    /// list alone would go on passing.
    ///
    /// The cwd is the session root, which is what makes the relative path work.
    #[test]
    fn a_later_command_sees_an_earlier_commands_effect() {
        let root = temp_root("sequence");
        let outcomes = run(
            &root,
            &[
                Command::new("echo one >> log"),
                Command::new("echo two >> log"),
                Command::new("cat log"),
            ],
            10_000,
        );
        assert_eq!(
            outcomes[2],
            DynamicOutcome::Ran {
                output: "one\ntwo".to_owned(),
                truncated: false,
                fell_back: false,
            },
            "the commands did not run in document order: {outcomes:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// BUG-185: the whole invocation has a ceiling, not just each command.
    ///
    /// Three legs, because the interesting claim is the middle one. A slot cap
    /// alone leaves 32 commands x the per-command timeout — 16 minutes on a
    /// `spawn_blocking` thread that connection teardown cannot cancel.
    #[test]
    fn the_invocation_budget_stops_commands_that_have_not_started() {
        let root = temp_root("budget");

        // Leg one — an exhausted budget starts nothing at all, proven by the
        // side effect rather than by the outcome list: a runner that ran the
        // command and then relabelled the outcome would pass on the list alone.
        let outcomes = run_within(
            &root,
            &[Command::new("echo leaked > budget-leg-one")],
            10_000,
            0,
        );
        assert!(
            matches!(&outcomes[0], DynamicOutcome::NotRun { reason } if reason.contains("budget")),
            "an exhausted budget is a not-run naming the budget: {outcomes:?}"
        );
        assert!(
            !root.join("budget-leg-one").exists(),
            "the command must never have started"
        );

        // Leg two — the real shape. The first command overruns the budget, so
        // the second never starts, and it is reported as NOT-RUN rather than
        // timed out: it was never launched, and calling it a timeout would
        // point the reader at the wrong command to fix.
        let outcomes = run_within(
            &root,
            &[
                Command::new("sleep 1"),
                Command::new("echo leaked > budget-leg-two"),
            ],
            10_000,
            300,
        );
        assert_eq!(
            outcomes[0],
            DynamicOutcome::TimedOut,
            "the running command is killed AT the budget, not allowed its own \
             full timeout past it — the remaining budget only ever shortens the \
             per-command deadline, which is what makes the total a real bound \
             rather than budget-plus-one-command: {outcomes:?}"
        );
        assert!(
            matches!(&outcomes[1], DynamicOutcome::NotRun { reason } if reason.contains("budget")),
            "and the one after it never starts — a not-run, not a timeout, \
             because it was never launched: {outcomes:?}"
        );
        assert!(
            !root.join("budget-leg-two").exists(),
            "and really never starts"
        );

        // Leg three — non-vacuity. With room, nothing is withheld; otherwise
        // legs one and two would pass on a runner that refused everything.
        let outcomes = run_within(&root, &[Command::new("echo fine")], 10_000, 60_000);
        assert_eq!(
            outcomes[0],
            DynamicOutcome::Ran {
                output: "fine".to_owned(),
                truncated: false,
                fell_back: false
            },
            "a budget with room runs the command: {outcomes:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The I/O edge: in order, one outcome per command, each arm typed —
    /// and a failing command does not stop the ones after it (BR-6, AC-10).
    /// **REQ-615 BR-6: the primary of a `cmd || fallback` is run and observed,
    /// and a succeeding primary reports no fallback.**
    ///
    /// This is the whole reason the daemon splits at all. Handed to one shell,
    /// `cat missing || echo none` exits 0 with `none` on stdout — byte-identical
    /// to a primary that succeeded and printed `none`. Only running the primary
    /// separately makes the two distinguishable.
    ///
    /// Mutation: delete the split in `run_one` and call `run_step` with the
    /// whole string — `fell_back` is `false` on the first row and it goes red.
    #[test]
    fn the_primary_of_a_fallback_command_is_run_and_observed() {
        let root = temp_root("fallback");
        std::fs::write(root.join("present.txt"), "real answer\n").unwrap();

        let outcomes = run(
            &root,
            &[
                Command::new("cat missing.txt 2>/dev/null || echo none"),
                Command::new("cat present.txt || echo none"),
            ],
            5_000,
        );

        match &outcomes[0] {
            DynamicOutcome::Ran {
                output, fell_back, ..
            } => {
                assert_eq!(output, "none");
                assert!(
                    *fell_back,
                    "the primary failed, so this output is a stand-in and must \
                     say so — the model must not read it as the project's answer"
                );
            }
            other => panic!("expected a ran fallback, got {other:?}"),
        }
        match &outcomes[1] {
            DynamicOutcome::Ran {
                output, fell_back, ..
            } => {
                assert_eq!(output, "real answer");
                assert!(!fell_back, "the primary succeeded; nothing fell back");
            }
            other => panic!("expected a ran primary, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-615 BR-6: the two steps of a split command share one deadline.**
    ///
    /// Splitting turned one `run_bounded` into two, and the obvious
    /// implementation hands each the full `timeout_ms` — which lets one `!cmd`
    /// slot take twice the per-command budget. That is the arithmetic BUG-185
    /// bounded, reopened by a change that is not about timeouts at all.
    ///
    /// # The assertion is on the outcome, not on the clock
    ///
    /// A wall-clock bound alone did not catch the mutation. A primary that eats
    /// the *whole* allowance is answered by the `checked_sub` guard before the
    /// remaining time is ever used, so `left` and `timeout_ms` are
    /// indistinguishable in that shape — the test passed against both. The
    /// fixture below instead leaves a **partial** remainder and gives the
    /// fallback a task that fits the full allowance but not the remainder, so
    /// the two spellings produce different `DynamicOutcome`s rather than
    /// different durations. 200 ms of slack either side of a 400 ms task.
    ///
    /// Mutation: pass `timeout_ms` to the fallback instead of `left` — the
    /// fallback then has 600 ms for a 400 ms sleep, returns `Ran("late")`, and
    /// this goes red.
    #[test]
    fn the_two_steps_of_a_split_command_share_one_deadline() {
        let root = temp_root("deadline");

        // Primary burns 400 ms of a 600 ms allowance and fails; the fallback
        // needs 400 ms and may have only ~200.
        let outcomes = run(
            &root,
            &[Command::new("sleep 0.4; false || sleep 0.4 && echo late")],
            600,
        );
        assert!(
            !matches!(&outcomes[0], DynamicOutcome::Ran { output, .. } if output == "late"),
            "the fallback ran to completion, so it was given a fresh allowance \
             rather than what the primary left: {:?}",
            outcomes[0]
        );

        // And the whole-allowance shape still ends at the deadline rather than
        // starting a second shell.
        let started = Instant::now();
        let whole = run(&root, &[Command::new("sleep 5 || echo fallback")], 400);
        let elapsed = started.elapsed();
        assert!(
            matches!(whole[0], DynamicOutcome::TimedOut),
            "{:?}",
            whole[0]
        );
        assert!(
            elapsed < Duration::from_millis(2_000),
            "one slot must not outlive its own deadline by running two shells: \
             {elapsed:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-615 BR-6: a quoted or absent `||` changes nothing.**
    ///
    /// The benign path, and it guards every skill preamble that was working
    /// before this REQ. A separator found inside quotes would split a command
    /// the shell treats as one, and running half of it is worse than not
    /// splitting at all.
    ///
    /// Mutation: make `split_top_level_or` quote-blind — the quoted row goes
    /// red with truncated output.
    #[test]
    fn a_quoted_or_absent_separator_changes_nothing() {
        let root = temp_root("nosplit");

        let outcomes = run(
            &root,
            &[
                Command::new("echo 'a || b'"),
                Command::new("echo plain"),
                Command::new("echo one | tr a-z A-Z"),
            ],
            5_000,
        );

        for (index, expected) in [(0usize, "a || b"), (1, "plain"), (2, "ONE")] {
            match &outcomes[index] {
                DynamicOutcome::Ran {
                    output, fell_back, ..
                } => {
                    assert_eq!(output, expected, "slot {index}");
                    assert!(!fell_back, "slot {index} did not fall back");
                }
                other => panic!("slot {index}: {other:?}"),
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_all_runs_in_document_order_and_types_every_outcome() {
        let root = temp_root("order");
        let outcomes = run(
            &root,
            &[
                Command::new("echo first"),
                Command::new("exit 3"),
                Command::new("echo third 1>&2"),
                Command::new("echo last"),
            ],
            10_000,
        );
        assert_eq!(outcomes.len(), 4, "one outcome per command");
        assert_eq!(
            outcomes[0],
            DynamicOutcome::Ran {
                output: "first".to_owned(),
                truncated: false,
                fell_back: false,
            }
        );
        assert_eq!(
            outcomes[1],
            DynamicOutcome::Failed {
                status: "exited 3".to_owned(),
                exit_status: Some(3),
            },
            "a non-zero exit is a failure, not output"
        );
        assert_eq!(
            outcomes[2],
            DynamicOutcome::Ran {
                output: String::new(),
                truncated: false,
                fell_back: false,
            },
            "stderr is not inlined; the command still succeeded"
        );
        assert_eq!(
            outcomes[3],
            DynamicOutcome::Ran {
                output: "last".to_owned(),
                truncated: false,
                fell_back: false,
            },
            "a failure never stops the commands after it"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A runaway command is killed on the deadline and the rest still run
    /// (AC-10 at this layer; the turn-level half is TASK-205's).
    #[test]
    fn a_command_past_the_deadline_times_out_and_the_next_one_still_runs() {
        let root = temp_root("timeout");
        let started = std::time::Instant::now();
        let outcomes = run(
            &root,
            &[Command::new("sleep 10"), Command::new("echo after")],
            200,
        );
        assert_eq!(outcomes[0], DynamicOutcome::TimedOut);
        assert_eq!(
            outcomes[1],
            DynamicOutcome::Ran {
                output: "after".to_owned(),
                truncated: false,
                fell_back: false,
            }
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "killed promptly, nowhere near the 10s sleep"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The output the expansion inlines is **capped**: `run_bounded` returns
    /// raw streams, and this is the caller that has to apply the ceiling
    /// (TASK-198 AC — leaving the cap in `render_output` would inline uncapped
    /// command output into a prompt).
    #[test]
    fn a_long_running_commands_output_is_capped_before_it_reaches_the_expansion() {
        let root = temp_root("cap");
        let outcomes = run(
            &root,
            &[Command::new("head -c 20000 /dev/zero | tr '\\0' x")],
            10_000,
        );
        let DynamicOutcome::Ran { output, .. } = &outcomes[0] else {
            panic!("expected a ran outcome, got {:?}", outcomes[0]);
        };
        assert!(
            output.chars().count() < 20_000,
            "20,000 characters reached the prompt uncapped"
        );
        assert!(
            output.contains("output truncated"),
            "the cap says how much it threw away: {output:.80}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A root that does not exist means the command **never started** — a
    /// not-run, not a failure, because the two get different placeholders.
    #[test]
    fn a_command_that_never_started_is_a_not_run() {
        let outcome = run_one(
            Path::new("/nonexistent-teton-skill-root"),
            &Command::new("echo hi"),
            10_000,
        );
        assert!(
            matches!(&outcome, DynamicOutcome::NotRun { reason } if reason.contains("failed to start")),
            "expected a not-run, got {outcome:?}"
        );
    }

    /// The three constructors the turn path uses, and the one property that
    /// matters about them: they are three different sentences.
    #[test]
    fn the_not_run_reasons_are_three_distinct_sentences() {
        assert_eq!(DynamicOutcome::declined().reason(), Some("declined"));
        assert_eq!(
            DynamicOutcome::not_run_at_plan().reason(),
            Some("plan permission level")
        );
        assert_eq!(
            DynamicOutcome::no_terminal().reason(),
            Some("no terminal, so no human could be asked")
        );
        assert_eq!(DynamicOutcome::TimedOut.reason(), Some("timed out"));
        assert_eq!(
            DynamicOutcome::Ran {
                output: "x".to_owned(),
                truncated: false,
                fell_back: false,
            }
            .reason(),
            None,
            "the arm with output has no reason"
        );
        assert!(DynamicOutcome::Ran {
            output: String::new(),
            truncated: false,
            fell_back: false,
        }
        .did_run());
        assert!(!DynamicOutcome::declined().did_run());
    }

    // -----------------------------------------------------------------------
    // REQ-619 BR-1 / BR-2 / BR-7: the preamble verdict
    // -----------------------------------------------------------------------

    /// **REQ-619 verify, m3 — a command the budget stopped is not classified.**
    ///
    /// The verdict used to be taken above the budget check, so every command of
    /// an over-budget invocation was classified whether or not it would be
    /// attempted. That is neither free nor harmless: `classify` may walk a
    /// subtree for up to 1.5 s per command, so a 32-slot invocation could spend
    /// three quarters of a minute classifying commands the budget had already
    /// stopped — spending it *after* the deadline the budget exists to hold, on
    /// a blocking-pool thread `spawn_blocking` cannot cancel. Deciding not to
    /// run something is not a reason to do work about it.
    ///
    /// The count is the assertion: **one classify call per attempted command**,
    /// not one per command in the body. The instrument is `run_all_with`'s
    /// classifier seam for the reason the sibling below gives — the real
    /// classifier answers the same thing however many times it is asked, so
    /// only a counting stand-in can tell "once" from "twice".
    ///
    /// # What replaces the verdict, and why it changes nothing
    ///
    /// `Verdict::not_classified()` — content-free and `Unknown`, the only
    /// honest thing to say about a command nothing looked at.
    /// [`super::provenance::fold_expansion`] ignores a `NotRun` command's
    /// verdict outright (BR-2), which is what makes the substitution invisible
    /// downstream; the last two assertions state the value at this level, so a
    /// future fold that *did* read it would meet a conservative answer rather
    /// than a stale one.
    ///
    /// # Mutation
    ///
    /// Ran with the `classify_with` call moved back above the budget check:
    /// **2 red** — this test on `a command the budget stopped is not
    /// classified` (2 calls where 1 was expected), and
    /// `an_unrun_command_is_classified_but_not_spawned` on its budget-door
    /// reason. Ran again with `Verdict::not_classified()` replaced by
    /// `classify_with(reach, command)` in the budget arm — the same defect
    /// spelled as a fallback — and the same two go red. Restored: green.
    #[test]
    fn a_command_the_budget_stopped_is_never_classified() {
        let root = project_root("budget-classify");
        let calls = std::cell::Cell::new(0usize);
        let counting = |reach: &PreambleReach, command: &Command| {
            calls.set(calls.get() + 1);
            preamble_verdict(reach, command)
        };

        let runs = run_all_with(
            &test_reach(&root),
            &[
                Command::new("sleep 1"),
                Command::new("echo never > budget-classify-leg"),
            ],
            10_000,
            300,
            &counting,
        );

        // Fixture: the first command really did overrun and the second really
        // was withheld, or the count below is about a run that never happened.
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].outcome, DynamicOutcome::TimedOut, "{runs:?}");
        assert!(
            matches!(&runs[1].outcome, DynamicOutcome::NotRun { reason } if reason.contains("budget")),
            "{runs:?}"
        );
        assert!(
            !root.join("budget-classify-leg").exists(),
            "the second command must never have started"
        );

        assert_eq!(
            calls.get(),
            1,
            "a command the budget stopped is not classified: the classifier may \
             walk a subtree for up to 1.5s per command, and spending that on a \
             command the daemon has already decided not to run spends it past \
             the very deadline the budget holds"
        );

        // The withheld command still carries a verdict — a half-record would be
        // worse — and it is the conservative one, content-free and `Unknown`.
        assert_eq!(runs[1].verdict.kind, VerdictKind::Unknown);
        assert!(
            runs[1].verdict.sources.is_empty() && !runs[1].verdict.out_of_root_touch,
            "an unclassified command proves nothing about any file: {:?}",
            runs[1].verdict
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-1: one verdict per command, and it is taken before that command
    /// spawns.**
    ///
    /// The instrument is `run_all_with`'s classifier seam, because the real
    /// classifier is a pure function of the command text: asked once or asked
    /// three times, before the spawn or after it, it answers the same thing, so
    /// a test built on its *answers* alone cannot tell those cases apart
    /// (LESSON-569 — verify the mechanism, do not reason about it). Here the
    /// injected classifier and the commands themselves append to **one file**,
    /// so the file is the interleaving: `classify-a, spawn-a, classify-b,
    /// spawn-b` and nothing else.
    ///
    /// The order is per command, not per invocation, and that is the design
    /// (ADR-619-1): each verdict is taken against the tree as it stands when
    /// its own command is about to run, exactly as `ShellTool::run` takes one
    /// against the tree in front of it.
    ///
    /// Mutation (run, red, reverted): taking the handed-out verdict *below*
    /// `run_one` in `run_all_with` — the verdict read off a tree the command
    /// has already changed — reorders and doubles the log lines. **1 red**,
    /// this test.
    #[test]
    fn the_verdict_is_taken_once_per_command_before_it_spawns() {
        let root = project_root("verdict-order");
        let log = root.join("log");
        let commands = [
            Command::new("echo spawn-a >> log"),
            Command::new("echo spawn-b >> log"),
        ];
        let recording = |reach: &PreambleReach, command: &Command| {
            use std::io::Write;
            let tag = command
                .as_str()
                .split("spawn-")
                .nth(1)
                .and_then(|rest| rest.chars().next())
                .expect("every fixture command names its own tag");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap();
            writeln!(file, "classify-{tag}").unwrap();
            preamble_verdict(reach, command)
        };

        let runs = run_all_with(
            &test_reach(&root),
            &commands,
            10_000,
            INVOCATION_BUDGET_MS,
            &recording,
        );

        // The fixture must really have spawned, or the ordering claim below is
        // about a runner that ran nothing.
        assert_eq!(runs.len(), commands.len());
        assert!(
            runs.iter().all(|run| run.outcome.spawned()),
            "both commands must run: {:?}",
            runs.iter().map(|run| &run.outcome).collect::<Vec<_>>()
        );

        let lines: Vec<String> = std::fs::read_to_string(&log)
            .expect("the classifier and the commands share one log")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines,
            ["classify-a", "spawn-a", "classify-b", "spawn-b"],
            "BR-1: each command's verdict is taken before that command spawns"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("classify-"))
                .count(),
            commands.len(),
            "BR-1: exactly one verdict per command — a per-arm verdict is what \
             BR-2 forbids"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-1: the two ends of REQ-614's grammar, reached from a preamble.**
    ///
    /// `ls -la` is the benign path — the shape a skill body actually contains,
    /// and the case that decides whether this REQ changed anything at all: it
    /// pinned every session before, and it is provable. `sh probe.sh` is the
    /// opaque verb, which stays exactly as unprovable as it was.
    ///
    /// Both spellings of the opaque case are here on purpose. `sh -c '…'` is
    /// rejected for its **quoting** before any verb table is consulted, so a
    /// test using only that spelling would pass with the `OPAQUE` table
    /// deleted — the reason assertion on `sh probe.sh` is what pins the verb.
    ///
    /// Mutation (run, red, reverted): passing `RootKind::Plain` instead of
    /// `reach.root_kind` in `preamble_verdict` makes every verdict `Unknown`
    /// (`the session root is not a project`). **4 red** — the `ls -la` arm
    /// here, plus `exit_status_and_output_never_change_the_verdict`,
    /// `a_preamble_reason_is_static_and_content_free` and
    /// `an_unrun_command_is_classified_but_not_spawned`, which pins the
    /// opaque-verb sentence and so notices a root that stopped being a
    /// project. Four, not the three predicted: the count is the finding
    /// (LESSON-640).
    #[test]
    fn an_opaque_verb_is_unknown_and_a_name_only_verb_is_rooted() {
        let root = project_root("verbs");
        std::fs::write(root.join("probe.sh"), "printf x\n").unwrap();
        let commands = [
            Command::new("ls -la"),
            Command::new("sh probe.sh"),
            Command::new("sh -c 'echo x'"),
        ];

        let runs = run_all(&test_reach(&root), &commands, 10_000);

        assert_eq!(
            runs[0].verdict.kind,
            VerdictKind::Rooted,
            "`ls -la` names no path outside the root: {}",
            runs[0].verdict.reason
        );
        assert_eq!(
            runs[1].verdict.kind,
            VerdictKind::Unknown,
            "an interpreter's reach is the whole machine"
        );
        assert_eq!(
            runs[1].verdict.reason, "the command runs an interpreter, build tool or network client",
            "the verb table is what rejects `sh probe.sh` — not its punctuation"
        );
        assert_eq!(runs[2].verdict.kind, VerdictKind::Unknown);
        assert!(
            runs.iter().all(|run| run.outcome.spawned()),
            "every fixture command must run, or these are verdicts about a \
             runner that did nothing: {:?}",
            runs.iter().map(|run| &run.outcome).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-2: output and exit status never change the verdict.**
    ///
    /// Four commands, four different outcomes, two verdicts: the three
    /// `READS_NOTHING` verbs are `Rooted` whether they exit zero, exit one or
    /// are killed on the deadline, and the interpreter is `Unknown` after
    /// printing nothing at all. That is the exit-code side channel REQ-585's
    /// verify found, closed by the verdict rather than by asking whether
    /// anything printed.
    ///
    /// Structural, not incidental: `classify` takes neither an exit status nor
    /// an output by signature, and `run_all_with` calls it before there is
    /// either. This test is what notices if that stops being true.
    ///
    /// Mutation (run, red, reverted): in `run_all_with`, recomputing the
    /// verdict after `run_one` as an `Unknown` for any outcome that is not
    /// `Ran` — the shape a "did it succeed?" branch would have. **1 red**,
    /// this test, on the `false` and `sleep 5` arms.
    #[test]
    fn exit_status_and_output_never_change_the_verdict() {
        let root = project_root("outcomes-do-not-move-verdicts");
        std::fs::write(root.join("probe.sh"), "exit 0\n").unwrap();
        let commands = [
            Command::new("true"),
            Command::new("false"),
            Command::new("sleep 5"),
            Command::new("sh probe.sh"),
        ];

        let runs = run_all(&test_reach(&root), &commands, 300);

        // The four arms really are four arms.
        assert!(
            matches!(&runs[0].outcome, DynamicOutcome::Ran { .. }),
            "{:?}",
            runs[0].outcome
        );
        assert!(
            matches!(&runs[1].outcome, DynamicOutcome::Failed { .. }),
            "{:?}",
            runs[1].outcome
        );
        assert_eq!(runs[2].outcome, DynamicOutcome::TimedOut);
        assert!(
            matches!(&runs[3].outcome, DynamicOutcome::Ran { output, .. } if output.is_empty()),
            "the unknown command must print nothing: {:?}",
            runs[3].outcome
        );

        for (index, run) in runs.iter().take(3).enumerate() {
            assert_eq!(
                run.verdict.kind,
                VerdictKind::Rooted,
                "command {index} exits differently and reaches no further: {}",
                run.verdict.reason
            );
            assert_eq!(
                run.verdict.reason, runs[0].verdict.reason,
                "one verdict, whatever the command came to"
            );
        }
        assert_eq!(
            runs[3].verdict.kind,
            VerdictKind::Unknown,
            "a command that printed nothing still pins: {}",
            runs[3].verdict.reason
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-2: a command the door left unrun carries a verdict, and is not
    /// spawned.**
    ///
    /// Two doors, and since REQ-619's verify (m3) they answer differently — on
    /// purpose:
    ///
    /// - the **invocation budget** (BUG-185), which `run_all_within` closes
    ///   itself. A command it stops is *not classified at all*: the check comes
    ///   first now, and the record is completed with
    ///   `Verdict::not_classified()`. Classifying it would mean walking a
    ///   subtree for up to 1.5 s per command, after the deadline the budget
    ///   exists to hold — see
    ///   [`a_command_the_budget_stopped_is_never_classified`], which counts the
    ///   calls. Nothing downstream can tell the difference, because
    ///   `fold_expansion` ignores a `NotRun` command's verdict outright.
    /// - the **consent** door, which never reaches the runner at all: the
    ///   caller builds the outcome (`door_outcome`) and classifies the command
    ///   it did not run with `preamble_verdict`. Here the *real* verdict is
    ///   kept, and BR-1 asks for it — the record the user sees names each
    ///   command's reach whether or not the door opened, and unlike the budget
    ///   arm this classification happens on a path with no deadline running.
    ///
    /// The two verdicts are therefore asserted separately below, and the
    /// difference between them is the finding this test now records.
    ///
    /// The control at the end is load-bearing: the same command, allowed to
    /// run, **does** create the sentinel. Without it "the sentinel is absent"
    /// would be satisfied by a fixture that could never have written anything
    /// (LESSON-640 — assert the arithmetic the fixture rests on).
    ///
    /// Mutations (run, red, reverted): (a) making `run_all_with`'s
    /// budget-exhausted arm call `run_one` anyway — the sentinel appears and
    /// this test fails on `!sentinel.exists()`. **2 red**, with
    /// `the_invocation_budget_stops_commands_that_have_not_started`.
    /// (b) replacing that arm's `Verdict::not_classified()` with a real
    /// `classify_with(reach, command)` — **2 red**, the consent-door
    /// discriminator here and the call count in
    /// [`a_command_the_budget_stopped_is_never_classified`].
    /// (Deleting the arm outright does not compile: `let …else` must diverge.
    /// Recorded rather than dropped — a mutation whose build never ran is not
    /// evidence.)
    #[test]
    fn an_unrun_command_is_classified_but_not_spawned() {
        let root = project_root("unrun");
        let sentinel = root.join("sentinel");
        std::fs::write(root.join("probe.sh"), "printf x > sentinel\n").unwrap();
        let command = Command::new("sh probe.sh");
        let reach = test_reach(&root);

        // The runner's own door: no budget left, so nothing starts.
        let held = run_all_within(&reach, std::slice::from_ref(&command), 10_000, 0);
        assert!(
            matches!(&held[0].outcome, DynamicOutcome::NotRun { reason } if reason.contains("budget")),
            "the budget must be the door here: {:?}",
            held[0].outcome
        );
        assert_eq!(held[0].verdict.kind, VerdictKind::Unknown);
        assert_eq!(
            held[0].verdict.reason,
            "the invocation's budget was spent before this command was classified",
            "REQ-619 verify m3: the budget arm does not classify, and the \
             verdict says which door it came through rather than borrowing a \
             classifier's sentence for work nobody did"
        );
        assert!(
            !sentinel.exists(),
            "a command held at the budget must not have spawned"
        );

        // The consent door, which the runner never sees — and which *does*
        // classify, because BR-1's record names every command's reach and no
        // deadline is running on this path.
        let declined = PreambleRun {
            verdict: preamble_verdict(&reach, &command),
            outcome: door_outcome(NotRunReason::Declined),
        };
        assert_eq!(
            declined.verdict.reason,
            "the command runs an interpreter, build tool or network client",
            "the consent door's classification is the real one, not a stand-in"
        );
        assert_ne!(
            declined.verdict.reason, held[0].verdict.reason,
            "the two doors are two answers: one classified the command, the \
             other decided not to spend the time"
        );
        assert_eq!(declined.outcome.reason(), Some("declined"));
        assert!(!sentinel.exists(), "classifying a command must not run it");

        // The control: the same command, allowed to run, writes the sentinel —
        // so the two absences above are absences of something possible.
        let ran = run_all(&reach, std::slice::from_ref(&command), 10_000);
        assert!(
            sentinel.exists(),
            "the fixture command must be able to leave a trace: {:?}",
            ran[0].outcome
        );
        assert_eq!(
            ran[0].verdict.reason, declined.verdict.reason,
            "running it changed nothing about its reach (BR-2) — the comparison \
             is against the *consent* door's verdict, which is the one that was \
             classified; the budget door's says only that nothing looked"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-7: the reason is one sentence from a closed set, and it carries
    /// neither the command nor its output.**
    ///
    /// The adversarial path (`benign_path: no`): a command whose text is
    /// distinctive, whose output holds a marker, and whose file is under a
    /// builtin boundary. All three reasons come back as the module's own
    /// literals, and the marker reaches the *outcome* — which is what makes
    /// "it is not in the reason" a statement about the reason rather than
    /// about a fixture that never had anything to leak.
    ///
    /// `let reason: &'static str` is the other half, and it is the half that
    /// cannot rot: a reason built from the command would not be `'static`, so
    /// the type is the guarantee and this line is where it is written down.
    ///
    /// Mutation (run, red, reverted): changing the `Rooted` reason in
    /// `classify_with_budget` to another sentence (`the command looked fine`).
    /// **1 red**, the first assertion here — the sentences are compared
    /// exactly, so nothing weaker than the closed set's own literal passes.
    #[test]
    fn a_preamble_reason_is_static_and_content_free() {
        let root = project_root("reasons");
        std::fs::write(
            root.join("ordinary-notes.txt"),
            "MARKER-a3f9-in-the-output\n",
        )
        .unwrap();
        std::fs::write(root.join(".env"), "MARKER-a3f9-in-a-boundary-file\n").unwrap();
        std::fs::write(root.join("probe.sh"), "printf x\n").unwrap();
        let commands = [
            Command::new("cat ordinary-notes.txt"),
            Command::new("cat .env"),
            Command::new("sh probe.sh"),
        ];

        let runs = run_all(&test_reach(&root), &commands, 10_000);

        assert_eq!(
            runs[0].verdict.reason,
            "every path the command names resolved inside the session root"
        );
        assert_eq!(
            runs[1].verdict.reason,
            "a path argument matches a privacy boundary"
        );
        assert_eq!(runs[1].verdict.kind, VerdictKind::BoundaryTouch);
        assert_eq!(
            runs[2].verdict.reason,
            "the command runs an interpreter, build tool or network client"
        );

        for (command, run) in commands.iter().zip(runs.iter()) {
            // The type, stated: a reason assembled from the command could not
            // be `'static`.
            let reason: &'static str = run.verdict.reason;
            assert!(
                !reason.contains(command.as_str()),
                "the reason quoted the command: {reason:?}"
            );
            assert!(
                !reason.contains("MARKER-a3f9"),
                "the reason carried file content: {reason:?}"
            );
            assert!(
                !reason.contains("ordinary-notes") && !reason.contains("probe.sh"),
                "the reason named a path argument: {reason:?}"
            );
        }

        // The marker really was there to leak.
        assert!(
            matches!(&runs[0].outcome, DynamicOutcome::Ran { output, .. }
                if output.contains("MARKER-a3f9")),
            "the fixture must actually produce the marker: {:?}",
            runs[0].outcome
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-7 / ADR-619-5: the wire record carries the verdict's *kind* and its
    /// *reason*, and nothing else the classifier or the command knew.**
    ///
    /// The adversarial half (`benign_path: no` in spirit, though the table
    /// lists this row as benign because the projection is the subject): a
    /// command whose run really does hold a marker, asserted to be there before
    /// it is asserted to be absent — the difference between a test and a
    /// fixture with nothing to leak. The claim is made against the **serialised
    /// event**, not against the struct's fields one at a time, because "no byte
    /// of the output reaches the wire" is a statement about the bytes and a
    /// per-field walk would silently stop covering a field somebody adds.
    ///
    /// `command` is the one place command text legitimately lives, and it is
    /// REQ-585's field, bounded, unchanged by this REQ — so it is asserted
    /// equal rather than merely absent.
    ///
    /// **REQ-619 TASK-401** deleted the verdict-free shim this test's last leg
    /// used to pin (`outcome_view_unclassified`): every production caller now
    /// holds a [`PreambleRun`], so there is no arm left that could answer
    /// `None` — or lie with `Rooted` — and the leg went with the shim.
    ///
    /// Mutation (run, red, reverted): mapping `VerdictKind::Unknown` to
    /// `PreambleReach::Rooted` in `outcome_view` — the reach assertion goes red, and
    /// the `/verbose` renderer would have gone silent on the one command that
    /// pinned the session. **1 red**, this test. Second mutation (run, red,
    /// reverted): building `reach_reason` as
    /// `format!("{} ({})", run.verdict.reason, command.as_str())` — the
    /// equality leg and the no-command-text leg both go red. **1 red**, this
    /// test.
    #[test]
    fn outcome_view_carries_the_verdict_and_nothing_of_the_output() {
        const MARKER: &str = "SECRET-OUTPUT-MARKER";

        let root = project_root("outcome-view");
        std::fs::write(root.join("probe.sh"), format!("printf '{MARKER}\\n'\n")).unwrap();
        std::fs::write(root.join("notes.txt"), "ordinary\n").unwrap();
        let opaque = Command::new("sh probe.sh");
        let rooted = Command::new("cat notes.txt");

        let runs = run_all(&test_reach(&root), std::slice::from_ref(&opaque), 10_000);
        let run = &runs[0];

        // The fixture is the one this claim needs: an *unknown* verdict beside
        // an outcome that really is carrying the marker.
        assert_eq!(run.verdict.kind, VerdictKind::Unknown);
        assert!(
            matches!(&run.outcome, DynamicOutcome::Ran { output, .. }
                if output.contains(MARKER)),
            "the fixture must actually have something to leak: {:?}",
            run.outcome
        );

        let view = outcome_view(&opaque, run, None);
        assert_eq!(
            view.reach,
            Some(teton_protocol::events::Reach::Unknown),
            "the kind crosses as itself"
        );
        assert_eq!(
            view.reach_reason.as_deref(),
            Some(run.verdict.reason),
            "the reason is the classifier's sentence verbatim, not a rewording"
        );
        assert_eq!(
            view.command, "sh probe.sh",
            "REQ-585's field is unchanged: the command text lives here, bounded, \
             and this REQ adds no second copy of it"
        );

        // The whole record, as it reaches a client. Nothing of the output, and
        // no second helping of the command.
        let wire = serde_json::to_string(&view).unwrap();
        assert!(
            !wire.contains(MARKER),
            "a byte of the command's output reached the event: {wire}"
        );
        assert_eq!(
            wire.matches("probe.sh").count(),
            1,
            "the command appears once, on `command`, and nowhere else: {wire}"
        );

        // The other side of the map: an ordinary in-root read is `rooted`, and
        // `/verbose` says nothing about it.
        let rooted_run = &run_all(&test_reach(&root), std::slice::from_ref(&rooted), 10_000)[0];
        assert_eq!(rooted_run.verdict.kind, VerdictKind::Rooted);
        assert_eq!(
            outcome_view(&rooted, rooted_run, None).reach,
            Some(teton_protocol::events::Reach::Rooted)
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
