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

use std::path::Path;
use std::process::ExitStatus;

use teton_protocol::events::{DynamicOutcomeView, NotRunReason};

use teton_protocol::methods::RefusalReason;

use crate::harness::permissions::SkillConsent;
use crate::harness::tools::shell::{cap_output, run_bounded, BoundedRun};

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
/// (`runtime::settle_dynamic_context`) and the model's `skill` tool
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
#[must_use]
pub fn outcome_view(
    command: &Command,
    outcome: &DynamicOutcome,
    door: Option<NotRunReason>,
) -> DynamicOutcomeView {
    DynamicOutcomeView {
        // File-supplied bytes on a surface: bounded and rendered on one line
        // here, at the same ceiling the fold's echoed placeholder uses (BR-3).
        command: teton_core::session_root::bounded_field(
            command.as_str(),
            crate::skills::expand::COMMAND_ECHO_MAX_CHARS,
        ),
        outcome: match outcome {
            DynamicOutcome::Ran { output, .. } => teton_protocol::events::DynamicOutcome::Ran {
                output_bytes: output.len() as u64,
                truncated: outcome.output_truncated(),
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

/// Run every command in order under the `shell` tool's jail, environment
/// scrub, `PATH` floor, process group and deadline — the single I/O edge of
/// this feature (BR-6, BR-14).
///
/// Sequential and in document order, because that is the order the user
/// consented to and the order the body reads in. `timeout_ms` is per command,
/// as it is for the `shell` tool.
///
/// The gate is **not** here: this function runs what it is given. Deciding
/// whether it may run at all is the caller's, under the skill's own permission
/// key (ADR-6), and a caller that decided *no* builds the [`DynamicOutcome`]s
/// itself rather than calling this.
#[must_use]
pub fn run_all(root: &Path, commands: &[Command], timeout_ms: u64) -> Vec<DynamicOutcome> {
    commands
        .iter()
        .map(|command| run_one(root, command, timeout_ms))
        .collect()
}

/// One command's trip through [`run_bounded`], typed for the fold.
fn run_one(root: &Path, command: &Command, timeout_ms: u64) -> DynamicOutcome {
    match run_bounded(root, command.as_str(), timeout_ms) {
        BoundedRun::Completed { status, stdout, .. } if status.success() => {
            // stdout only. BR-6 says stdout enters the expansion; a command
            // whose diagnostics matter can redirect them itself (`2>&1`), and
            // inlining stderr would put a linter's warnings in a prompt the
            // author asked for a file listing in.
            let text = String::from_utf8_lossy(&stdout).trim_end().to_owned();
            let (output, _raw_chars, truncated) = cap_output(text);
            DynamicOutcome::Ran { output, truncated }
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
        let outcomes = run_all(
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
            },
            "the commands did not run in document order: {outcomes:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The I/O edge: in order, one outcome per command, each arm typed —
    /// and a failing command does not stop the ones after it (BR-6, AC-10).
    #[test]
    fn run_all_runs_in_document_order_and_types_every_outcome() {
        let root = temp_root("order");
        let outcomes = run_all(
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
            },
            "stderr is not inlined; the command still succeeded"
        );
        assert_eq!(
            outcomes[3],
            DynamicOutcome::Ran {
                output: "last".to_owned(),
                truncated: false,
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
        let outcomes = run_all(
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
        let outcomes = run_all(
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
            }
            .reason(),
            None,
            "the arm with output has no reason"
        );
        assert!(DynamicOutcome::Ran {
            output: String::new(),
            truncated: false,
        }
        .did_run());
        assert!(!DynamicOutcome::declined().did_run());
    }
}
