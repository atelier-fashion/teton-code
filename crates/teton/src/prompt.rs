//! The interactive-input seam.
//!
//! Anything that reads a line from the user goes through a [`Prompter`], so the
//! permission round-trip (event in → question → answer → `permission/respond`
//! out) can be unit-tested with scripted answers and no terminal. The binary
//! wires in [`StdinPrompter`]; tests wire in a scripted one.
//!
//! It is also where the terminal's *mode* is changed and put back: the echo-off
//! guard behind the credential prompt, the raw-mode guard the event pump holds
//! for the length of a turn, and the one process-wide slot a signal handler
//! restores from before re-raising (REQ-622 ADR-622-3). Every `libc` call and
//! every `unsafe` block in this crate is in this file, which is the property
//! that makes "the terminal seam" a place rather than a habit.

use std::cell::UnsafeCell;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;

// The pump's wake interval, imported rather than restated: it is the length of
// the blocking wait between two empty passes of the raw answer read
// ([`read_answer_raw`]), and a second copy of the figure here would be a
// second answer to "how long may that loop sleep" — free to drift from the
// pump's. It is declared in `client.rs`, beside the loop that waits on it
// (REQ-621).
use crate::client::FRAME_INTERVAL;
use crate::input_editor::{Edit, InputEditor};
use crate::render::defused;

/// A source of interactive answers.
pub trait Prompter {
    /// Show `question` and read one line of input. Returns `None` on EOF (the
    /// user pressed Ctrl-D), which callers treat as a cancel.
    fn ask(&mut self, question: &str) -> Option<String>;

    /// Show `question` and read one line **that is not shown back** — the
    /// credential prompt (REQ-572 ADR-3, AC-5).
    ///
    /// Same contract as [`Self::ask`] in every other respect: one line, trimmed
    /// of its terminator, `None` on EOF.
    ///
    /// **Deliberately has no default implementation.** A default would have to
    /// be `ask`, and a `Prompter` that forgot to override it would echo a
    /// credential into the user's scrollback and into any pty capture of the
    /// session while looking exactly like a working one. There are three
    /// implementors and they all live in this module; a fourth — the ratatui
    /// front-end the `Surface`/`Prompter` seams exist for — must answer this
    /// question explicitly rather than inherit the wrong answer silently.
    fn ask_secret(&mut self, question: &str) -> Option<String>;
}

/// An explicit yes, and nothing else (LESSON-470). Empty and EOF are both no.
///
/// It lives beside [`Prompter`] because it is the other half of the same seam:
/// every default-no confirmation in this crate asks through the trait above and
/// reads the answer through this function. It was written out three times —
/// `/web setup`, `/provider setup`, `/provider test` — byte for byte, which is
/// three chances for one of them to grow a "sure" or an "ok" the other two do
/// not have, and no test that could see the drift. LESSON-528's rule, applied to
/// a predicate small enough to look harmless: a predicate worth copying is worth
/// exposing, and consent is the last thing that may be spelled differently in
/// different rooms.
pub(crate) fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// The real prompter: writes the question to stdout and reads a line from stdin.
///
/// **Every question is [`defused`] on the way out** (REQ-573). A question is not
/// always a fixed string composed by this binary: it can carry a tool name the
/// daemon sent, or — since the `/web setup` catalog moved daemon-side — an
/// auth-header template that arrived over RPC. Those reach a terminal that reads
/// control characters as *commands*, exactly like the text a
/// [`crate::render::Surface`] guards, and this writer is the one path to the
/// screen that does not go through one. The transform is imported rather than
/// re-implemented: two sanitizers is one sanitizer plus a gap.
#[derive(Debug, Default)]
pub struct StdinPrompter {
    /// The buffer this prompter assembles an answer in while raw mode is
    /// engaged (REQ-622 ADR-622-2).
    ///
    /// **Deliberately not the pump's editor**, and that is the whole of BR-5.
    /// The pump's editor holds the line the user was part-way through typing
    /// when the question opened; it is `shelve`d where the question is *drawn*
    /// (TASK-417) so that nothing typed before that moment can be read as an
    /// answer. This field is the other half of the same rule, one layer down:
    /// the prompter reads into a buffer of its own, reset by
    /// [`read_answer_raw`] at the top of every read, so "a question reads only
    /// keystrokes typed after it was drawn" is true by construction rather
    /// than by the pump having remembered to shelve. Threading the pump's
    /// editor down here would have made it true by *agreement between two
    /// callers*, which is the shape LESSON-502 warns about.
    ///
    /// It is a whole [`InputEditor`] and not a `String` because the assembling
    /// is the part that must not be duplicated (BR-2): which bytes are one
    /// character, which byte was a Backspace, which control byte is dropped —
    /// one implementation, reached by both readers.
    answer: InputEditor,
}

impl StdinPrompter {
    /// A new stdin-backed prompter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Prompter for StdinPrompter {
    /// One line, read the way the terminal's current mode requires.
    ///
    /// **Two readers, one seam** (REQ-622 BR-2). Between turns the terminal is
    /// canonical and this is the `read_line` it has always been — byte for
    /// byte, which is what keeps every piped fixture and every scripted flow
    /// unchanged (BR-1, AC-7). Inside a turn at a terminal the kernel is no
    /// longer assembling lines, so a `read_line` here would sit waiting for a
    /// newline the line discipline will never deliver; the raw branch reads
    /// the keystrokes itself, through [`Self::answer`], and echoes them into
    /// the question's own row.
    ///
    /// Raw mode is read off the process-wide slot ([`RawMode::is_engaged`])
    /// rather than handed in, because it is a fact about the terminal and not
    /// about any one caller — the alternative was a flag threaded through
    /// nineteen `UiContext` construction sites, eighteen of which would have
    /// had to guess (ADR-622-2).
    fn ask(&mut self, question: &str) -> Option<String> {
        let mut out = io::stdout();
        let question = defused(question);
        let _ = write!(out, "{question}");
        let _ = out.flush();
        if RawMode::is_engaged() {
            // The pump's rows are withdrawn and its pending line shelved
            // before a question is drawn (TASK-417), so from here until this
            // returns the terminal is the prompter's alone (BR-13).
            let answered = read_answer_raw(
                &mut self.answer,
                &question,
                &mut out,
                read_available,
                || stdin_ready(FRAME_INTERVAL),
            );
            // `ECHO` is off, so the user's Enter painted nothing and the cursor
            // is still at the end of the answer. Without this the next row the
            // session writes would land beside the question — `ask_secret`'s
            // reason, one flag word over. Written on the cancel path too: the
            // cursor is mid-row whichever way the read ended.
            let _ = writeln!(out);
            let _ = out.flush();
            return answered;
        }
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => None, // EOF
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
            Err(_) => None,
        }
    }

    fn ask_secret(&mut self, question: &str) -> Option<String> {
        let mut out = io::stdout();
        let _ = write!(out, "{}", defused(question));
        let _ = out.flush();
        // **Never read a credential in raw mode** (REQ-622, verify). This
        // prompter's hidden read is a canonical-mode `read_line`: with `ICANON`
        // clear the kernel hands bytes over as they arrive and `read_line` would
        // sit waiting for a newline the line discipline is no longer going to
        // synthesize — and the guard restoring afterwards would restore the
        // *raw* settings, because they are what it saved. Unreachable today (a
        // key prompt is only opened between turns, where nothing is engaged) and
        // cheap insurance against the day a setup flow is reached from inside
        // one. The refusal is the echo-off refusal, deliberately: the user-facing
        // fact is identical — nothing was read, nothing was stored, fix the
        // terminal — and a second notice for a state that cannot happen would be
        // a second sentence to keep true.
        if RawMode::is_engaged() {
            return refuse_secret(&mut out);
        }
        // Engaged for exactly the read and restored by the guard's `Drop`, on
        // every path out — including the error one.
        let hidden = match EchoOff::engage() {
            EchoState::Hidden(guard) => Some(guard),
            // Nothing to switch off: stdin is a pipe, so the terminal was never
            // going to paint anything. The same read, unhidden because there is
            // no screen to hide it from.
            EchoState::NoTerminal => None,
            // Fail **closed**. Somebody is at a terminal and the terminal would
            // paint every character of the credential into their scrollback.
            // Reading anyway is the failure mode this branch exists to refuse:
            // it looks exactly like a working prompt and leaks the key. It is
            // also where a restore slot already held by the turn's `RawMode`
            // arrives, since a refused arm is a refused engage — the backstop
            // behind the explicit check above.
            EchoState::Failed => return refuse_secret(&mut out),
        };
        let mut line = String::new();
        let read = io::stdin().read_line(&mut line);
        let was_hidden = hidden.is_some();
        drop(hidden);
        if was_hidden {
            // The terminal echoed nothing, the user's own Enter included, so the
            // cursor is still sitting at the end of the question. Without this
            // the next line the session draws would land beside the prompt.
            let _ = writeln!(out);
            let _ = out.flush();
        }
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }
}

/// The key prompt's fail-closed refusal, written once (REQ-572 AC-5, REQ-622
/// verify).
///
/// A blank line to leave the question's own row, the notice, and `None`. Two
/// callers reach it — a terminal that would not hide the typing, and a terminal
/// already in raw mode — and the bytes must be the same for both, because what
/// the user has to be told is the same: nothing was read and nothing was stored.
fn refuse_secret(out: &mut impl Write) -> Option<String> {
    let _ = writeln!(out);
    let _ = writeln!(out, "{ECHO_UNAVAILABLE}");
    let _ = out.flush();
    None
}

/// Terminal echo, switched off for the life of the guard and restored on drop
/// (REQ-572 AC-5).
///
/// Canonical mode is deliberately left alone: only `ECHO` is cleared, so the
/// kernel still assembles the line and `read_line` behaves exactly as it does
/// for every other prompt — the one difference is that the characters are not
/// painted back.
///
/// **The residual is retired** (REQ-622 ADR-622-3). A signal that killed the
/// process between `engage` and the drop — Ctrl-C at the key prompt — used to
/// leave the user's terminal with echo off until they ran `stty sane`, because
/// `Drop` does not run for a process the kernel terminates. REQ-572 accepted
/// that window on the grounds that installing a handler from a CLI with no
/// other signal handling would be a larger change than the wart it removed;
/// REQ-622 needs the handler anyway, for a mode change the user would notice a
/// great deal more than a missing echo, and one slot serves both guards. So
/// `engage` now registers the saved settings in [`RESTORE`] and
/// [`install_restore_handlers`] arms the `sigaction` handler that replays them
/// before re-raising: `SIGINT`, `SIGTERM` and `SIGHUP` all put the terminal
/// back on the way out (BR-7, AC-6). The ordinary abort paths — EOF, an empty
/// answer — still return through the guard and restore in `Drop`.
struct EchoOff {
    /// The terminal settings as they were, to be put back verbatim.
    saved: libc::termios,
}

/// What [`EchoOff::engage`] found — three states, because two of them are safe
/// to read a credential through and one is not.
enum EchoState {
    /// Echo is off for the life of the guard.
    Hidden(EchoOff),
    /// There is no terminal echo to switch off: stdin is not a tty, so nothing
    /// was ever going to be painted.
    NoTerminal,
    /// Stdin **is** a terminal and echo could not be cleared.
    Failed,
}

/// [`EchoState`] without the guard — the shape the rule is stated in, so it can
/// be asserted with no terminal in the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EchoOutcome {
    Hidden,
    NoTerminal,
    Failed,
}

/// The fail-closed rule, from the three facts the syscalls report.
///
/// Pure, and separate from [`EchoOff::engage`], because the branch that matters
/// is the one a test process cannot otherwise reach: a real tty whose
/// `tcgetattr`/`tcsetattr` fails. Folding it into the `unsafe` block would leave
/// the security-relevant decision assertable only by breaking a terminal.
///
/// **Closed, not open.** A tty that would not clear `ECHO` yields `Failed` and
/// the caller refuses to read, rather than reading with the characters painted
/// back. The only state that reads unhidden is the one where there was nothing
/// to hide from.
fn classify_echo(is_tty: bool, got_attrs: bool, set_attrs: bool) -> EchoOutcome {
    if !is_tty {
        return EchoOutcome::NoTerminal;
    }
    if got_attrs && set_attrs {
        EchoOutcome::Hidden
    } else {
        EchoOutcome::Failed
    }
}

/// What a terminal that will not hide the typing is told.
///
/// It names the thing that did not happen (nothing was read), the reason, and
/// the two ways out — because a user who has just been refused a prompt needs to
/// know whether to retype or to fix their terminal.
const ECHO_UNAVAILABLE: &str =
    "error: this terminal would not turn echo off, so the key would have been shown as you \
     typed it and left in your scrollback — nothing was read and nothing was stored. Run `stty \
     sane` and try again, or use a different terminal.";

impl EchoOff {
    /// Switch echo off, and say which of the three states that landed in.
    fn engage() -> EchoState {
        // SAFETY: `isatty` reads a descriptor number; `tcgetattr` and
        // `tcsetattr` read and write a single owned `termios` through the
        // pointer and touch nothing else. Every failure is reported through the
        // return code, which is checked. Same shape as the `poll` and
        // `TIOCGWINSZ` calls in this module.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        let got_attrs =
            is_tty && unsafe { libc::tcgetattr(libc::STDIN_FILENO, &raw mut saved) } == 0;
        // Armed **before** the change, for [`RawMode::engage`]'s reason: a
        // signal delivered between the arm and the `tcsetattr` finds the slot
        // holding settings that are still in effect and writes them over
        // themselves, which costs nothing. Arming afterwards would leave a
        // window — short, but exactly the window Ctrl-C lands in — with echo
        // off and no undo anywhere in the process.
        let guard = got_attrs.then(|| Self::arm(saved)).flatten();
        // `guard.is_some()`, not `got_attrs`: [`RawMode::engage`]'s rule for
        // its reason — a change nothing can undo must not be made.
        let set_attrs = guard.is_some() && {
            let mut hidden = saved;
            hidden.c_lflag &= !libc::ECHO;
            // TCSAFLUSH: anything typed ahead of the prompt is discarded rather
            // than read as the start of a credential — the standard posture for
            // a password prompt, and the one that keeps a stray paste out of the
            // keychain.
            let rc =
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw const hidden) };
            rc == 0
        };
        match classify_echo(is_tty, got_attrs, set_attrs) {
            EchoOutcome::Hidden => match guard {
                Some(guard) => EchoState::Hidden(guard),
                // Unreachable by construction: `Hidden` requires `got_attrs`,
                // which is what produced the guard. Fail closed rather than
                // unwrap — an impossible state must not panic a key prompt.
                None => EchoState::Failed,
            },
            EchoOutcome::NoTerminal => EchoState::NoTerminal,
            // Any guard bound above drops as this function returns: it writes
            // back settings that were never changed and disarms the slot, so a
            // refusal leaves the terminal and the slot as `engage` found them.
            EchoOutcome::Failed => EchoState::Failed,
        }
    }

    /// Arm the process-wide restore slot with `saved` and hand back the guard
    /// that disarms it.
    ///
    /// The twin of [`RawMode::arm`], and split out for its reasons: the arm and
    /// the `Drop` that clears it are one named pair, and the bookkeeping is
    /// then assertable without aiming a `tcsetattr` at descriptor 0 —
    /// which, under `cargo test` from a terminal, is the developer's own
    /// (`both_guards_arm_and_clear_the_restore_slot`).
    ///
    /// `None` when another guard already holds the slot, [`RawMode::arm`]'s
    /// answer for its reason. Here the caller's mapping is fail-**closed**:
    /// [`EchoState::Failed`] means the credential is not read at all.
    fn arm(saved: libc::termios) -> Option<Self> {
        install_restore_handlers();
        RESTORE
            .store(&saved, SLOT_ECHO_OFF)
            .then_some(Self { saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: as in `engage` — one owned `termios`, by pointer, and the
        // result is deliberately unused because a failure here has no remedy
        // and must not panic a drop.
        unsafe {
            // TCSANOW, not TCSAFLUSH: the line the user just submitted has been
            // read, and discarding whatever they typed after it would eat the
            // next answer of the very flow that asked for this one.
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.saved);
        }
        // Disarmed **after** the restore, never before. A signal delivered
        // between the two finds the slot still armed and writes back settings
        // that are already in effect — a second no-op. Clearing first would
        // leave a window in which the slot says "nothing to put back" while
        // the terminal is still changed, which is the one ordering that loses
        // the restore this whole mechanism exists to perform. Named, for
        // [`RawMode`]'s reason one guard over.
        RESTORE.clear(SLOT_ECHO_OFF);
    }
}

/// The entry-area prompter for an interactive session: the question sits
/// between a dim horizontal rule above and below, so the place to type reads
/// as its own space rather than as the next log line.
///
/// The choreography is plain ANSI: draw all three rows (rule, empty input row,
/// rule), move the cursor up two rows into the input row, show the question,
/// and read. The terminal's own echo of Enter lands the cursor on the bottom
/// rule, and one newline steps past it, so whatever prints next starts clean
/// below the frame. (A line of input longer than the terminal wraps over the
/// bottom rule — the line-based cost of not being a full-screen TUI yet; the
/// ratatui `Surface` this UI is written against will own the frame properly.)
///
/// With `framed: false` it behaves exactly like [`StdinPrompter`] — the
/// non-interactive path stays byte-identical.
#[derive(Debug)]
pub struct FramedStdinPrompter {
    /// Draw the frame at all? Off when stdout is not a terminal.
    framed: bool,
    /// Dim the rules (and nothing else) when colour is on.
    color: bool,
    /// The status row's content, or `None` for no row (REQ-560).
    ///
    /// Content only — composed by [`crate::status::status_line`], which is a
    /// pure function with no terminal. This type owns *placement*, which is the
    /// half that needs one.
    status: Option<String>,
    /// How many rows [`Self::draw`] actually emitted **below** the bottom rule.
    ///
    /// The matched half of the frame's geometry, and the reason it is a field
    /// rather than a recomputation: `draw` writes the rows and `read_line` has
    /// to step past exactly the rows that were written. Deriving it twice from
    /// [`Self::status`] would be two answers to one question, and the one that
    /// drifted would strand a row.
    ///
    /// Deliberately **separate** from `erase`'s `status_rows`, which counts rows
    /// drawn *above* the frame (REQ-556's loading indicator). One count serving
    /// both directions strands one of them (REQ-560 BR-11).
    below_rows: usize,
    /// The buffer an answer is assembled in while raw mode is engaged.
    ///
    /// [`StdinPrompter`]'s field twin, for its reasons, and a field of its own
    /// rather than a shared one because the two prompters are two objects and a
    /// question is never open at both.
    ///
    /// Reachable, and not decoration: the entry frame is only *drawn* between
    /// turns, but "raw mode is engaged" is a fact about the terminal rather
    /// than about the caller, and a prompter that reached for `read_line` while
    /// the kernel had stopped assembling lines would hang rather than fail.
    /// Both implementors of the seam answer the question explicitly —
    /// `ask_secret`'s paragraph, applied to the other half of the trait.
    answer: InputEditor,
}

impl FramedStdinPrompter {
    /// A new entry prompter. `framed` gates the frame, `color` the dimming.
    #[must_use]
    pub fn new(framed: bool, color: bool) -> Self {
        Self {
            framed,
            color,
            status: None,
            below_rows: 0,
            answer: InputEditor::default(),
        }
    }

    /// Set the status row's content for the next [`Self::draw`], or clear it.
    ///
    /// Takes composed content rather than the state it is composed from: what
    /// the row says is [`crate::status`]'s decision and is unit-tested there
    /// with no terminal in the way (REQ-560 BR-8). A `None` — which is what a
    /// terminal too narrow for the row yields — means no row at all, and the
    /// frame is the three rows it always was.
    pub(crate) fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    /// The horizontal rule sized to the terminal, dimmed when colour is on.
    fn rule(&self) -> String {
        let bar = "\u{2500}".repeat(terminal_width());
        if self.color {
            format!("\x1b[2m{bar}\x1b[0m")
        } else {
            bar
        }
    }
}

impl FramedStdinPrompter {
    /// Draw the entry frame and park the cursor in its input row.
    ///
    /// Split out of [`Prompter::ask`] so an interactive caller can keep the
    /// frame *open* while it waits on both stdin and the daemon's event stream
    /// (REQ-556 BR-1). `ask` remains draw-then-read, so every existing caller
    /// and the whole non-interactive path are byte-identical.
    pub(crate) fn draw(&mut self, question: &str) {
        if !self.framed {
            return;
        }
        let bytes = self.draw_bytes(question, "");
        let mut out = io::stdout();
        let _ = write!(out, "{bytes}");
        let _ = out.flush();
    }

    /// Draw the entry frame with `line` already in its input row, then step the
    /// cursor past the frame as a typed Enter would (REQ-622 ADR-622-5).
    ///
    /// What a **queued** line looks like on the way out. A line typed during a
    /// turn is not printed while the turn runs (BR-4); the one place it is ever
    /// shown is here, in the frame of the prompt that is about to send it, so
    /// the user sees what is going out exactly once and in the place they would
    /// have typed it.
    ///
    /// The echo is ours, not the terminal's, and that is the whole of the
    /// second half: nothing was typed, so nothing was echoed, so the cursor is
    /// still sitting in the input row at the end of the line — [`Self::erase`]'s
    /// EOF starting point, and it needs the same two newlines plus
    /// [`Self::below_rows`]. Getting that wrong does not misplace one row; it
    /// leaves the next line of output overwriting the frame's bottom rule.
    pub(crate) fn draw_submitted(&mut self, question: &str, line: &str) {
        if !self.framed {
            return;
        }
        let bytes = self.submitted_bytes(question, line);
        let mut out = io::stdout();
        let _ = write!(out, "{bytes}");
        let _ = out.flush();
    }

    /// Exactly what [`Self::draw_submitted`] writes.
    ///
    /// Split out for [`Self::draw_bytes`]' reason, which this REQ leans on
    /// harder than REQ-560 did: a queued line's frame is the *only* place a
    /// queued line is ever shown, so "shown once, in the input row, with the
    /// cursor left where Enter would have left it" has to be assertable with no
    /// terminal in the room (REQ-560 BR-11).
    fn submitted_bytes(&mut self, question: &str, line: &str) -> String {
        let mut bytes = self.draw_bytes(question, line);
        bytes.push_str(&self.advance_bytes(true));
        bytes
    }

    /// The question as it reaches the terminal: defused, then tinted.
    ///
    /// One transform with two callers — the frame, and the answer row a raw-mode
    /// read repaints ([`read_answer_raw`]). Two copies of it would be two
    /// sanitizers, which is one sanitizer plus a gap, and the styling half has
    /// already been got wrong once in exactly this file (see
    /// [`Self::draw_bytes`]).
    fn styled(&self, question: &str) -> String {
        let question = defused(question);
        // Styling happens HERE, after defusing, or not at all. A caller
        // hand-composing SGR into the question hands the sanitizer exactly the
        // bytes it exists to destroy — the REQ-573 verify pass briefly shipped
        // that as literal `[36m` debris where the chevron's tint had been. The
        // seam owns the tint; callers hand in plain text.
        if self.color {
            question.replace('›', "\x1b[36m›\x1b[0m")
        } else {
            question
        }
    }

    /// Exactly what [`Self::draw`] writes, and the place [`Self::below_rows`] is
    /// decided.
    ///
    /// Split out so the frame's geometry — the part that stands a row up or
    /// strands it — is assertable without a terminal (REQ-560 BR-11). `draw`
    /// itself is then a `write!` of this, which is the only part that needs one.
    ///
    /// The question is [`defused`] here for [`StdinPrompter`]'s reason, and the
    /// frame sharpens it: this composition's own escapes move the cursor by
    /// exactly the rows it drew, so a question carrying its own `\x1b[…A` or a
    /// bare `\r` would step the cursor somewhere the geometry does not know
    /// about and shred the frame it sits in.
    ///
    /// `prefill` is what the input row already holds — `""` for the frame a user
    /// is about to type into, and a **queued** line for the frame that shows
    /// what is about to be sent (REQ-622 ADR-622-5). One parameter rather than a
    /// second composing function, so the frame keeps one geometry: a queued
    /// line's frame differs from an empty one by the text after the cursor
    /// escape and by nothing else, which is what makes [`Self::advance_bytes`]'
    /// count valid for both. It is defused for the question's reason — the
    /// user's own line is still text a terminal reads as commands.
    ///
    /// The input row is still drawn **blank** and the prefill written after the
    /// cursor has risen into it, rather than composed into that row. The rows
    /// above and below are measured by the escape this function emits, and a
    /// prefill composed into the row would put the bottom rule one row further
    /// down than the count can see the moment it wrapped.
    fn draw_bytes(&mut self, question: &str, prefill: &str) -> String {
        let question = self.styled(question);
        let rule = self.rule();
        // Rule, blank input row, rule, then the status row if there is one.
        let mut bytes = format!("{rule}\n\n{rule}\n");
        self.below_rows = match &self.status {
            Some(status) => {
                // Enum-derived content today, defused anyway: the first status
                // field that carries daemon- or model-supplied text would
                // otherwise reopen the seam this function just closed.
                bytes.push_str(&defused(status));
                bytes.push('\n');
                1
            }
            None => 0,
        };
        // The cursor is now at the start of the row after everything drawn. Two
        // rows up is the input row when nothing sits below the bottom rule, and
        // one further up per below-row — the count this same call just wrote,
        // which is what keeps the pair matched.
        let up = 2 + self.below_rows;
        bytes.push_str(&format!("\x1b[{up}A{question}{}", defused(prefill)));
        bytes
    }

    /// Erase a frame drawn by [`Self::draw`], leaving the cursor where ordinary
    /// output should resume.
    ///
    /// Needed because a notice rendered while the frame is open would land in
    /// the input row and shred it. The caller erases, renders, and draws again
    /// — so the frame appears to stay put while lines scroll above it.
    ///
    /// The cursor sits in the input row, one row below the top rule, so the
    /// frame alone is one row up. `status_rows` is how many extra rows a caller
    /// drew *above* the frame (REQ-556's indicator draws one, or none when it
    /// has nothing to say) — they are erased together, because they were drawn
    /// together and a partial erase would leave a stale indicator stranded
    /// above the redrawn frame.
    ///
    /// **`status_rows` counts rows above the frame only, and REQ-560's status
    /// row below the bottom rule is not among them** — the two directions are
    /// counted independently, because one count serving both would strand
    /// whichever it was not measuring (BR-11). The below-row still goes: `\x1b[J`
    /// erases from the cursor to the end of the *screen*, so everything drawn
    /// below is already inside what this clears. That is why moving a row below
    /// the frame changed [`Self::draw_bytes`] and [`Self::advance_bytes`] but
    /// left this function alone.
    pub(crate) fn erase(&mut self, status_rows: usize) {
        if !self.framed {
            return;
        }
        let up = 1 + status_rows;
        let mut out = io::stdout();
        let _ = write!(out, "\r\x1b[{up}A\x1b[J");
        let _ = out.flush();
    }

    /// Read one line from the open frame, doing the cursor bookkeeping the
    /// frame's geometry needs. `None` on EOF.
    pub(crate) fn read_line(&mut self) -> Option<String> {
        let mut out = io::stdout();
        let mut line = String::new();
        let read = io::stdin().read_line(&mut line);
        if self.framed {
            let _ = write!(
                out,
                "{}",
                self.advance_bytes(matches!(read, Ok(0) | Err(_)))
            );
            let _ = out.flush();
        }
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }

    /// The newlines that step the cursor from wherever the read left it to where
    /// ordinary output should resume.
    ///
    /// Two starting points, because the terminal's own echo differs:
    ///
    /// - **Enter** echoed a newline, so the cursor is on the bottom rule — one
    ///   row from clear.
    /// - **EOF** (Ctrl-D) echoed nothing, so the cursor is still in the input
    ///   row — two rows from clear.
    ///
    /// Two, not three, and REQ-622 is what makes that worth saying: its two new
    /// callers both take the *second* starting point, and for the same reason
    /// EOF does rather than for a new one. A queued line
    /// ([`Self::draw_submitted`]) was painted by us and never typed, and an
    /// answer read in raw mode had `ECHO` switched off — in both cases the
    /// terminal echoed no newline, so the cursor is where a keystroke left it
    /// and not one row down. The parameter is named for the fact rather than
    /// for the keystroke it used to describe.
    ///
    /// Both then have to clear [`Self::below_rows`] more, and that is the whole
    /// of REQ-560's stranding hazard: with a status row below the bottom rule,
    /// the pre-REQ single newline would have parked the cursor **on** the status
    /// row and let the next output overwrite it in place, leaving whatever was
    /// wider than that output stranded behind it. The count comes from the same
    /// field [`Self::draw_bytes`] set, so the rows stepped over are exactly the
    /// rows written.
    fn advance_bytes(&self, no_echoed_newline: bool) -> String {
        let rows = if no_echoed_newline { 2 } else { 1 } + self.below_rows;
        "\n".repeat(rows)
    }
}

impl Prompter for FramedStdinPrompter {
    /// The frame, then one line read the way the terminal's mode requires.
    ///
    /// The raw branch is [`StdinPrompter::ask`]'s, with the frame around it:
    /// the answer is assembled by [`Self::answer`] and echoed into the input
    /// row the frame just drew, and the cursor is then stepped past the frame
    /// as [`Self::draw_submitted`] steps it — because in raw mode the Enter
    /// that ended the line painted nothing.
    fn ask(&mut self, question: &str) -> Option<String> {
        if !self.framed {
            return StdinPrompter::new().ask(question);
        }
        self.draw(question);
        if RawMode::is_engaged() {
            // The styled question is what is *on* the row, so it is what a
            // repaint has to put back — the same bytes `draw_bytes` just wrote,
            // from the same transform.
            let question = self.styled(question);
            let mut out = io::stdout();
            let answered = read_answer_raw(
                &mut self.answer,
                &question,
                &mut out,
                read_available,
                || stdin_ready(FRAME_INTERVAL),
            );
            let _ = write!(out, "{}", self.advance_bytes(true));
            let _ = out.flush();
            return answered;
        }
        self.read_line()
    }

    /// A credential is never asked for inside the entry frame.
    ///
    /// The frame's geometry is built around a line the terminal echoes back into
    /// the input row (`advance_bytes` steps over exactly the rows the echo
    /// produced); a read with echo off writes nothing there, so the frame would
    /// be stepped through wrong. It is also the wrong *place*: a credential
    /// question is dialogue, like the permission and model prompts, and those go
    /// to the session's plain prompter. This delegates rather than reimplements,
    /// so the hiding has one implementation.
    fn ask_secret(&mut self, question: &str) -> Option<String> {
        StdinPrompter::new().ask_secret(question)
    }
}

/// Wait up to `timeout` for stdin to have something to read (REQ-556 BR-1).
///
/// This is what lets the interactive entry loop stop *blocking* on stdin
/// without stopping being the only thing that *reads* it. A stdin reader thread
/// would have been a second reader of the same descriptor, and
/// `Connection::dispatch_event` answers permission and model-proposal prompts
/// with their own `read_line` — so a line typed while a consent prompt was open
/// would have gone to whichever reader the kernel woke (ADR-556-1). One reader,
/// interruptible wait, no race.
///
/// Returns `true` when a subsequent read will not meaningfully block: either
/// bytes are available or the descriptor is at EOF (`POLLIN` reports both, and
/// the caller distinguishes them by reading zero bytes). An error — `EINTR`
/// most often — reports `false`, which costs one tick and re-polls. It must
/// never be reported as EOF, or a stray signal would end the session.
#[must_use]
pub(crate) fn stdin_ready(timeout: std::time::Duration) -> bool {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `poll` reads `events` and writes `revents` through the pointer to
    // a single owned `pollfd` and touches nothing else; failure is reported
    // through the return code, which is checked here. Same shape as the
    // `TIOCGWINSZ` call below.
    let rc = unsafe { libc::poll(&raw mut fds, 1, ms) };
    rc > 0
}

/// Read whatever stdin has ready **without waiting**, or nothing.
///
/// The other half of [`stdin_ready`], and what lets the event pump be the
/// reader as well as the waiter (ADR-622-1): the tick that already asks whether
/// stdin has bytes now takes them. One `poll` with a zero timeout, then exactly
/// one `read(2)` when it says yes — never a second, so a tick can never sit on
/// the descriptor waiting for the rest of a line that is not coming.
///
/// `Ok(0)` means *nothing was read*, and it deliberately does not distinguish
/// "no bytes waiting" from "the descriptor is at EOF". Mid-turn the two mean
/// the same thing to the caller — do nothing this tick — because Ctrl-D is
/// inert during a turn by design (BR-15); EOF keeps its meaning only at an open
/// prompt, where a blocking read reports it.
///
/// `EINTR` is `Ok(0)` for [`stdin_ready`]'s reason: a stray signal costs one
/// tick, and must never look like input or like the end of the session.
pub fn read_available(buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() || !stdin_ready(std::time::Duration::ZERO) {
        return Ok(0);
    }
    // SAFETY: `read` writes at most `buf.len()` bytes into the slice's own
    // allocation through the pointer and touches nothing else; the length
    // handed over is the slice's own. Failure is reported through the return
    // code, which is checked below. Same shape as the `poll` call above.
    let n = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
        )
    };
    // A negative return is the only failure `read` has, so the conversion is
    // the check: anything that fits a `usize` is a byte count.
    match usize::try_from(n) {
        Ok(read) => Ok(read),
        Err(_) => {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                Ok(0)
            } else {
                Err(err)
            }
        }
    }
}

/// Throw away whatever the kernel is still holding on stdin, at the moment a
/// question is drawn (REQ-622 BR-5).
///
/// **The window this closes is the kernel's, not the editor's.** The pump has
/// just read every byte the descriptor had and folded it into the pending line,
/// and [`InputEditor::shelve`] has just moved that line aside — so everything
/// the user legitimately typed is already accounted for and already out of the
/// way. What can still be sitting in the terminal's input queue is what arrived
/// in the microseconds between that read and this call: keystrokes aimed at a
/// question that was not on the screen yet. BR-5 says a question reads only
/// what was typed after it was drawn, and the shelve alone cannot say that for
/// those bytes, because they are not in the editor at all — they are in the
/// kernel, and the question's first `read(2)` would take them.
///
/// `TCIFLUSH`: the *input* queue only. The output queue is the turn's own rows
/// and dropping it would erase the question being asked.
///
/// **Ungated, like the shelve it follows.** On a pipe or a closed stdin
/// `tcflush` fails with `ENOTTY` and does nothing, so a piped session is
/// byte-identical (BR-1, AC-7); at a terminal in canonical mode — REQ-622's
/// BR-11 fallback — the kernel's queue is a *line the user submitted before the
/// question existed*, which is precisely what BR-5 forbids from answering it.
/// One rule, both modes, no reachability argument standing between the rule and
/// the code.
///
/// Under `cfg(test)` this counts instead of flushing. `cargo test` runs with
/// `STDIN_FILENO` on whichever terminal launched it, so a real `tcflush` in a
/// unit test would throw away what the *developer* had typed into their own
/// shell — [`RawMode`]'s scripted-engage hook, one call over.
pub(crate) fn discard_type_ahead() {
    #[cfg(test)]
    TYPE_AHEAD_FLUSHES.with(|calls| calls.set(calls.get() + 1));
    #[cfg(not(test))]
    // SAFETY: `tcflush` takes a descriptor number and a queue selector and
    // touches nothing this process owns. The result is deliberately unused:
    // a terminal that will not discard its input queue is the ordinary
    // `ENOTTY` of a pipe, and there is nothing useful to say about it at the
    // moment a question is being drawn.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

/// How many bytes one pass of [`read_answer_raw`] takes from the terminal.
///
/// One `read(2)` per pass, so this is the largest paste a single pass can
/// swallow; anything longer arrives over several passes, which the editor's
/// partial-byte accumulator already handles.
const ANSWER_CHUNK: usize = 256;

/// How many consecutive "stdin says it is readable and hands over nothing"
/// passes [`read_answer_raw`] reads as the end of the descriptor.
///
/// The two states `read_available` deliberately does not distinguish have to be
/// told apart *here*, because this loop is the one place where the difference
/// matters: a question that keeps waiting on a hung-up terminal never returns,
/// and a question that treated one interrupted read as EOF would cancel itself
/// on a stray `SIGWINCH`. `POLLIN` with a zero-byte read is EOF or a signal;
/// eight of them in a row, with no wall clock spent between (a hung-up
/// descriptor polls readable immediately), is EOF.
const READY_BUT_EMPTY_IS_EOF: usize = 8;

/// One answer, read as keystrokes and echoed into the question's own row
/// (REQ-622 BR-2, BR-3, BR-5).
///
/// The prompter's half of ADR-622-2, and the reason it is a free function with
/// its arguments handed in: the loop — *read, decode, echo, stop at Enter* — is
/// identical for both prompters, and every terminal fact it needs is a
/// parameter. `keys` is [`read_available`] in production and a scripted byte
/// source in a test; `wait` is a [`stdin_ready`] of one frame; `out` is stdout.
/// So the behaviour that BR-5 is about is assertable with no terminal, no
/// keyboard and no clock (BR-10).
///
/// **The buffer is reset here, at the top, on every read.** That is BR-5 by
/// construction rather than by discipline: whatever is in `answer` when this is
/// called — the tail of a previous question, a byte that arrived between the
/// pump's shelve and the question's first row — is not part of this answer and
/// is dropped. It is then [`InputEditor::shelve`]d immediately, which is what
/// makes Enter *hold* the line for [`InputEditor::take_answer`] instead of
/// pushing it onto the queue: a question's answer must never become a queued
/// prompt (BR-5, BR-6).
///
/// **A byte at a time, stopping at the first Enter.** A paste of three lines
/// answered at a question is one answer and two lines nobody asked for; pushing
/// the whole read in one call would leave the second and third lines appended
/// to the answer in the shelved buffer, because a shelved editor's Enter does
/// not clear `pending`. Feeding the editor byte by byte lets the loop stop at
/// the moment the answer is complete and drop the rest of that read — the same
/// rule [`InputEditor::unshelve`] states for what a question leaves behind.
///
/// Returns `None` for a cancel — a read error, or a descriptor at EOF — which
/// is the contract [`Prompter::ask`] already has.
fn read_answer_raw(
    answer: &mut InputEditor,
    question: &str,
    out: &mut impl Write,
    mut keys: impl FnMut(&mut [u8]) -> io::Result<usize>,
    mut wait: impl FnMut() -> bool,
) -> Option<String> {
    *answer = InputEditor::default();
    answer.shelve();
    let mut buf = [0u8; ANSWER_CHUNK];
    let mut empty = 0usize;
    loop {
        let read = match keys(&mut buf) {
            // Clamped rather than trusted, as `RowState::read_keys` clamps its
            // own: production cannot report more than the buffer it was handed,
            // and a test hook that did would panic the slice below rather than
            // fail an assertion.
            Ok(read) => read.min(buf.len()),
            // A failed read mid-question is a cancel, not something to retry:
            // the caller's `None` path declines the request, which is the safe
            // answer to every question this prompter asks.
            Err(_) => return None,
        };
        if read == 0 {
            // Nothing this pass. Block for one frame rather than spin: this is
            // the same wait the pump uses, so a question open mid-turn costs
            // one `poll` per interval and no CPU.
            if wait() {
                empty += 1;
                if empty >= READY_BUT_EMPTY_IS_EOF {
                    return None;
                }
            } else {
                empty = 0;
            }
            continue;
        }
        empty = 0;
        for &byte in &buf[..read] {
            let mut moved = false;
            let mut complete = false;
            for edit in answer.push(&[byte]) {
                match edit {
                    Edit::Pending => moved = true,
                    // Enter, against a shelved buffer: the line is this
                    // question's answer and the queue was left alone.
                    Edit::Queued(_) => complete = true,
                    // A dropped control byte or a consumed escape sequence:
                    // nothing to repaint, and nothing echoed (BR-9).
                    Edit::Nothing => {}
                }
            }
            if moved {
                let _ = write!(
                    out,
                    "{}",
                    answer_row_bytes(question, answer.answer_so_far())
                );
                let _ = out.flush();
            }
            if complete {
                return Some(answer.take_answer());
            }
        }
    }
}

/// The question's input row, repainted with `answer` typed into it.
///
/// Composed from the **editor's** text rather than from the bytes the reader
/// happened to hand over, which is BR-2 at the writer: a row painted from raw
/// input would be a second decoder, and the one that drifted would paint half a
/// character or echo a Backspace as a glyph.
///
/// `\r\x1b[K` then the whole row, rather than an incremental append, because
/// Backspace has to *remove* a character and the row is the only thing here that
/// knows how wide the removed one was. `question` arrives already defused (and
/// tinted, at the framed prompter); the answer is defused here for
/// [`InputEditor::row`]'s reason — a bidi override is printable UTF-8, so it
/// reaches the line as an ordinary character and it is the *rendering* that has
/// to neutralize it.
///
/// A question and answer wider than the terminal wrap, and a repaint then lands
/// on the wrong row: the same line-based cost [`FramedStdinPrompter`] documents
/// for its own input row, and it goes away with the full-screen surface that
/// seam exists for.
fn answer_row_bytes(question: &str, answer: &str) -> String {
    format!("\r\x1b[K{question}{}", defused(answer))
}

/// The terminal's column count, or a conservative 80 when stdout is not a
/// terminal or the query fails.
///
/// `pub(crate)` since REQ-560: the status row's content function needs the width
/// to decide whether the row fits, and takes it as a parameter so that decision
/// stays pure (BR-8). This is the one place the width is *queried*.
pub(crate) fn terminal_width() -> usize {
    // SAFETY: TIOCGWINSZ writes a plain `winsize` struct through the pointer
    // and touches nothing else; a failure is reported through the return code
    // and leaves the zeroed struct untouched.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &raw mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

// ---------------------------------------------------------------------------
// The terminal's mode, and an undo a signal can find (REQ-622 ADR-622-1/3)
// ---------------------------------------------------------------------------

/// The terminal out of canonical mode for the life of the guard, and back into
/// it on drop (REQ-622 ADR-622-1).
///
/// The sibling of [`EchoOff`], and deliberately as small a change. `ICANON` and
/// `ECHO` are cleared and `VMIN`/`VTIME` set to zero, so a read returns
/// immediately with whatever bytes are there and the kernel paints none of them
/// — which is the whole of what the client needs in order to be the thing that
/// assembles the line. Everything else in the four flag words is left exactly
/// as found: `ISIG` above all, so Ctrl-C is still a `SIGINT` and BR-8 holds by
/// construction rather than by handling; then `ICRNL` and `OPOST`, so Enter
/// still arrives as `\n` and every row a [`crate::render::Surface`] writes
/// still ends the way it does today. The arithmetic itself is [`raw_from`]'s,
/// where it is assertable with no terminal in the room.
pub struct RawMode {
    /// The terminal settings as they were, to be put back verbatim.
    saved: libc::termios,
}

/// What [`RawMode::engage`] found — three states, and the turn runs in all
/// three.
///
/// The shape is [`EchoState`]'s, and the *polarity* is the opposite one:
/// nothing here refuses. See [`classify_raw`].
pub enum RawOutcome {
    /// The terminal is out of canonical mode for the life of the guard.
    Raw(RawMode),
    /// Stdin is not a terminal, so there was no canonical mode to leave. A
    /// piped session reaches this without a single `tcsetattr` (BR-1, AC-7).
    NoTerminal,
    /// Stdin **is** a terminal and the mode change was refused. The caller
    /// keeps canonical mode, keeps REQ-621's abandon-on-submit path armed, and
    /// says so once (BR-11).
    Failed,
}

/// [`RawOutcome`] without the guard — the shape the rule is stated in, so it
/// can be asserted with no terminal in the room.
///
/// The same pairing as [`EchoState`]/[`EchoOutcome`], with the names the other
/// way round: `RawOutcome` is what `engage` hands back, and this is its
/// guard-free twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawVerdict {
    Raw,
    NoTerminal,
    Failed,
}

/// The fail-**open** rule, from the same three facts [`classify_echo`] reads.
///
/// Pure, and separate from [`RawMode::engage`], for [`classify_echo`]'s reason:
/// the branch that matters is the one a test process cannot otherwise reach —
/// a real tty whose `tcgetattr`/`tcsetattr` fails.
///
/// **Open, not closed, and the two polarities sit a few lines apart on
/// purpose.** The truth tables are identical; what differs is what the caller
/// does with `Failed`. `EchoOff`'s caller refuses to read, because a terminal
/// that will not stop painting would paint a credential into the user's
/// scrollback: there is something to hide, so failing to hide it means not
/// reading. Here there is nothing to hide. The bytes at stake are the user's
/// own prompt on their own screen, and the fallback is not a leak but *last
/// month's behaviour* — the kernel assembles the line, REQ-621's
/// abandon-on-submit path stays armed, and the turn runs. Refusing a turn
/// because the terminal would not give up canonical mode would trade a
/// cosmetic regression for a dead CLI. So `Failed` here earns a notice
/// (BR-11), not a refusal.
///
/// It is still a state of its own, and not folded into [`RawVerdict::NoTerminal`]:
/// both mean "carry on", but only one of them is a terminal that refused, and
/// only that one earns the notice.
fn classify_raw(is_tty: bool, got_attrs: bool, set_attrs: bool) -> RawVerdict {
    if !is_tty {
        return RawVerdict::NoTerminal;
    }
    if got_attrs && set_attrs {
        RawVerdict::Raw
    } else {
        RawVerdict::Failed
    }
}

/// The settings [`RawMode::engage`] writes, from the settings it read.
///
/// Split out so the two-flag claim is arithmetic a test can do rather than a
/// terminal a test needs (`raw_mode_changes_only_icanon_and_echo`, BR-1). Every
/// bit this function does *not* touch is a bit the pty suite would otherwise be
/// the only witness to, and "we changed only two flags" is exactly the kind of
/// claim that stays true in the prose and stops being true in the code.
fn raw_from(saved: libc::termios) -> libc::termios {
    let mut raw = saved;
    // The two flags, and only these two: the kernel stops assembling lines and
    // stops painting keystrokes, because the client now does both.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    // With `ICANON` clear, a read is governed by `VMIN`/`VTIME`, and the pump
    // must never block: zero of each means "return with whatever is there,
    // immediately", which is what makes [`read_available`] a poll-then-read
    // rather than a wait.
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

impl RawMode {
    /// Leave canonical mode, and say which of the three states that landed in.
    #[must_use]
    pub fn engage() -> RawOutcome {
        // SAFETY: `isatty` reads a descriptor number; `tcgetattr` and
        // `tcsetattr` read and write a single owned `termios` through the
        // pointer and touch nothing else. Every failure is reported through the
        // return code, which is checked. The same shape as `EchoOff::engage`,
        // one flag word over.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        let got_attrs =
            is_tty && unsafe { libc::tcgetattr(libc::STDIN_FILENO, &raw mut saved) } == 0;
        // Armed **before** the change, not after. A signal delivered between
        // the arm and the `tcsetattr` finds the slot holding settings that are
        // still in effect and writes them over themselves, which costs
        // nothing. Arming afterwards would leave a window — short, but exactly
        // the window Ctrl-C lands in — in which the terminal is raw and
        // nothing in the process knows how to undo it.
        let guard = got_attrs.then(|| Self::arm(saved)).flatten();
        // `guard.is_some()`, not `got_attrs`: an arm the slot refused must not
        // be followed by a `tcsetattr`, because nothing in the process would
        // then know how to undo it (REQ-622, verify).
        let set_attrs = guard.is_some() && {
            let engaged = raw_from(saved);
            // TCSAFLUSH, as at the key prompt: whatever was typed before the
            // turn began is discarded rather than arriving as the first
            // keystrokes of a line the user has not started. The `Drop` uses
            // TCSANOW, for the mirror-image reason.
            let rc =
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw const engaged) };
            rc == 0
        };
        match classify_raw(is_tty, got_attrs, set_attrs) {
            RawVerdict::Raw => match guard {
                Some(guard) => RawOutcome::Raw(guard),
                // Unreachable by construction: `Raw` requires `got_attrs`,
                // which is what produced the guard. Fail open rather than
                // unwrap — an impossible state must not panic mid-turn.
                None => RawOutcome::Failed,
            },
            RawVerdict::NoTerminal => RawOutcome::NoTerminal,
            // Fail open. Any guard bound above drops as this function returns:
            // it writes back settings that were never changed and disarms the
            // slot, so a refusal leaves the terminal and the slot exactly as
            // `engage` found them.
            RawVerdict::Failed => RawOutcome::Failed,
        }
    }

    /// Arm the process-wide restore slot with `saved` and hand back the guard
    /// that disarms it.
    ///
    /// Separate from [`Self::engage`] so the arm and the `Drop` that clears it
    /// are one named pair, and so the bookkeeping is assertable without aiming
    /// a `tcsetattr` at descriptor 0 — which, under `cargo test` from a
    /// terminal, is the developer's own
    /// (`both_guards_arm_and_clear_the_restore_slot`).
    /// `None` when another guard already holds the slot — see
    /// [`RestoreSlot::store`]. The terminal is **not** changed in that case:
    /// [`Self::engage`] reads this answer before its `tcsetattr`, so a refused
    /// arm leaves the terminal exactly as it found it and reports BR-11's
    /// fail-open `Failed`.
    fn arm(saved: libc::termios) -> Option<Self> {
        install_restore_handlers();
        RESTORE.store(&saved, SLOT_RAW).then_some(Self { saved })
    }

    /// Whether the terminal is out of canonical mode **right now**.
    ///
    /// Read off the slot rather than threaded through the `UiContext`
    /// construction sites, because it is a property of the terminal and not of
    /// any one caller (ADR-622-2).
    ///
    /// The echo-off key prompt arms the same slot and this still answers
    /// `false`, which is the whole reason the slot records *which* guard holds
    /// it: echo off is canonical mode with the painting switched off, and a
    /// reader that took the raw path against it would sit waiting for bytes the
    /// kernel will not hand over until Enter.
    #[must_use]
    pub fn is_engaged() -> bool {
        RESTORE.armed_by() == SLOT_RAW
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: as in `engage` — one owned `termios`, by pointer, and the
        // result is deliberately unused because a failure here has no remedy
        // and must not panic a drop.
        unsafe {
            // TCSANOW, not TCSAFLUSH: the bytes the user typed while the turn
            // was ending are theirs, and discarding them here would eat the
            // start of the next prompt's line — `EchoOff`'s reason, at the
            // other end of the same turn.
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.saved);
        }
        // Disarmed **after** the restore, never before — `EchoOff`'s `Drop`
        // says why, and it is the same window. Named, so a guard that never
        // held the slot cannot disarm the one that does.
        RESTORE.clear(SLOT_RAW);
    }
}

// ---- The restore slot, and the handler that replays it (ADR-622-3) --------

/// Nothing is armed: the process owes the terminal no setting.
const SLOT_EMPTY: u8 = 0;
/// A [`RawMode`] guard holds the slot.
const SLOT_RAW: u8 = 1;
/// An [`EchoOff`] guard holds the slot.
const SLOT_ECHO_OFF: u8 = 2;

/// One saved `termios` and a word saying who owes it, readable from a signal
/// handler.
///
/// **Why not a `Mutex`.** The reader of this slot is a signal handler, and a
/// handler may only call what POSIX puts on its async-signal-safe list.
/// `pthread_mutex_lock` is not on it, and the reason is not pedantry: the
/// signal can be delivered to the very thread that already holds the lock —
/// `RawMode::engage` is one store away from its `tcsetattr` — and the handler
/// would then deadlock a process the user has just asked to die, with the
/// terminal still raw. A `RwLock`, a `OnceLock<Mutex<_>>` and every channel
/// have the same defect. An atomic load and a plain read of an
/// [`UnsafeCell`] are wait-free and allocate nothing, which is the whole of
/// what a handler is allowed to be.
///
/// **Why a word and not a bool.** Two questions are asked of the owner field
/// and the answers differ: the handler asks *is there anything to put back*
/// (either guard), and [`RawMode::is_engaged`] asks *is the terminal out of
/// canonical mode* (only [`RawMode`]). One bool would have to answer both, and
/// the reading that made the handler right would make a raw read of a
/// canonical terminal look correct.
struct RestoreSlot {
    /// [`SLOT_EMPTY`], [`SLOT_RAW`] or [`SLOT_ECHO_OFF`]. Written last on the
    /// way in — it is what publishes `saved` — and first on the way out.
    owner: AtomicU8,
    /// The settings to put back, initialised only while `owner` is not
    /// [`SLOT_EMPTY`].
    ///
    /// A [`MaybeUninit`] because a `static` cannot hold a `const`-constructed
    /// `libc::termios`, and because a zeroed one would be a lie: an unarmed
    /// slot has no settings, and "all zeroes" is a thing that could be written
    /// to a terminal rather than a thing that cannot.
    saved: UnsafeCell<MaybeUninit<libc::termios>>,
}

// SAFETY: every write to `saved` is ordered before the `SeqCst` store that
// publishes it, and every read is ordered after a `SeqCst` load that observed
// that store; the only writers are the two guards, which ADR-622-3 forbids
// from being engaged at once (debug-asserted in `RestoreSlot::store`). The
// signal handler only reads, and only when `owner` says the cell holds a value.
unsafe impl Sync for RestoreSlot {}

/// The one slot.
///
/// A single global on purpose: two terminal guards are never engaged at once
/// (ADR-622-3) — a question inside a turn reuses the turn's raw mode, and the
/// key prompt is only reachable between turns — so a stack of them would never
/// be more than one deep, and would be a second thing for a signal handler to
/// get wrong.
static RESTORE: RestoreSlot = RestoreSlot {
    owner: AtomicU8::new(SLOT_EMPTY),
    saved: UnsafeCell::new(MaybeUninit::uninit()),
};

impl RestoreSlot {
    /// Which guard holds the slot, or [`SLOT_EMPTY`].
    ///
    /// The one reader of the word, so the handler, [`RawMode::is_engaged`] and
    /// the tests all ask the same question the same way.
    fn armed_by(&self) -> u8 {
        self.owner.load(Ordering::SeqCst)
    }

    /// Arm the slot with the settings a guard has just saved, or refuse
    /// because another guard already holds it.
    ///
    /// **A refusal in every build, not a `debug_assert`** (REQ-622, verify).
    /// ADR-622-3 says two guards are never engaged at once, and that is true of
    /// today's control flow — a question inside a turn reuses the turn's raw
    /// mode, and the key prompt is only reachable between turns. A
    /// `debug_assert` turned that into "a release build overwrites the first
    /// guard's saved settings with the second's", which is the one outcome
    /// nobody would choose: the outer guard's `Drop` would then write back
    /// settings that were *already changed*, and the user's terminal would be
    /// left in the inner guard's mode with nothing in the process that knows
    /// how to undo it. The reachability argument is a reason not to expect
    /// this, never a reason to make the unexpected case destructive.
    ///
    /// The caller maps a refusal to its own fail-open or fail-closed outcome —
    /// [`RawOutcome::Failed`] and [`EchoState::Failed`] respectively — which is
    /// the same answer each already gives for a terminal that would not take
    /// the change. `#[must_use]`, so a future third guard cannot arm the slot
    /// by ignoring the answer.
    #[must_use]
    fn store(&self, saved: &libc::termios, owner: u8) -> bool {
        // A plain load and a store rather than a compare-exchange, and the
        // difference does not matter here: the only writers are guards on this
        // process's main thread (a `Prompter` and the event pump both run
        // there), so there is no second thread to lose a race with. What this
        // is defending against is a *code path*, not a thread.
        if self.armed_by() != SLOT_EMPTY {
            return false;
        }
        // SAFETY: at most one guard holds the slot at a time — the check above
        // is what makes that true rather than merely intended — so this is the
        // only writer; and a handler reads the cell only after the store below
        // publishes it, so it cannot observe the write in progress. The write
        // initialises the cell with a copy of a `termios` the caller owns.
        unsafe { (*self.saved.get()).write(*saved) };
        // The data first, the flag second, always: a handler that saw the flag
        // before the write landed would read an uninitialised cell.
        self.owner.store(owner, Ordering::SeqCst);
        true
    }

    /// Disarm the slot **if `owner` is the guard holding it**.
    ///
    /// The saved settings stay in the cell and simply become unreadable, which
    /// is all "cleared" can mean for a word a handler may only load.
    ///
    /// The owner check is the other half of [`Self::store`]'s refusal (REQ-622,
    /// verify): a guard that was refused the slot must not disarm it on the way
    /// out, or the refusal would have cost the *holder* its restore — a
    /// `Drop` order away from exactly the residual this mechanism exists to
    /// close. A compare-exchange rather than a load-then-store so the two halves
    /// cannot be separated by a later edit.
    fn clear(&self, owner: u8) {
        let _ = self
            .owner
            .compare_exchange(owner, SLOT_EMPTY, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Whether the slot holds `expected`, field by field — `libc::termios` has
    /// no `PartialEq`, and the four flag words plus the control characters are
    /// what a restore actually writes.
    ///
    /// Only meaningful while the slot is armed: an unarmed slot's cell is
    /// uninitialised, which is what the [`MaybeUninit`] is saying.
    #[cfg(test)]
    fn holds(&self, expected: &libc::termios) -> bool {
        assert_ne!(self.armed_by(), SLOT_EMPTY, "an unarmed slot holds nothing");
        // SAFETY: `owner` is not `SLOT_EMPTY`, which is published only after
        // the cell has been written, so it holds an initialised `termios`;
        // this copies it out on the thread that wrote it.
        let stored = unsafe { (*self.saved.get()).assume_init() };
        stored.c_iflag == expected.c_iflag
            && stored.c_oflag == expected.c_oflag
            && stored.c_cflag == expected.c_cflag
            && stored.c_lflag == expected.c_lflag
            && stored.c_cc == expected.c_cc
    }
}

/// The signals that end the process and would otherwise end it with the
/// terminal still changed (BR-7): the Ctrl-C a user presses, the `SIGTERM` a
/// wrapper or an init system sends, the `SIGHUP` a closed terminal window
/// sends, and the `SIGQUIT` that is Ctrl-\ on the very keyboard whose mode this
/// guard has changed.
///
/// `SIGQUIT` was added at REQ-622's verify pass and belongs here for the same
/// reason `SIGINT` does, only more so: it is a *keystroke*, sat under the
/// user's other hand while they type into a raw terminal, and its default
/// action is to kill the process — so the one key most likely to be hit by
/// accident during exactly the state this mechanism guards was the one signal
/// that left the terminal raw. Every signal here re-raises after restoring, so
/// `SIGQUIT` still dumps core where the system is configured to.
const RESTORE_SIGNALS: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Put the terminal back, then die of the signal that arrived.
///
/// **Every line of this function is on POSIX's async-signal-safe list, and
/// that is the specification rather than a nicety.** `tcsetattr`, `sigaction`
/// and `raise` are listed. `malloc`, every lock, every formatter and every
/// `println!` are not, and there is no allocation, no formatting, no lock and
/// no branch here that could reach one — a handler that took a lock could be
/// delivered to the thread already holding it and would hang a process the
/// user has just asked to die, with the terminal still raw.
///
/// The last two lines are what keeps Ctrl-C's meaning (BR-8): resetting the
/// disposition to `SIG_DFL` and re-raising hands the signal back to the kernel,
/// so the process dies *of* `SIGINT`, with `SIGINT`'s exit status and every
/// parent-visible semantic it has today. A handler that called `exit(130)`
/// instead would be describing that status rather than having it. The signal is
/// blocked while its own handler runs, so the `raise` is delivered as the
/// handler returns and the mask is lifted.
extern "C" fn restore_and_reraise(sig: libc::c_int) {
    if RESTORE.armed_by() != SLOT_EMPTY {
        // SAFETY: `owner` is not `SLOT_EMPTY`, which is published only after
        // the cell has been written, so it holds an initialised `termios`;
        // `tcsetattr` reads it through the pointer and writes only the
        // terminal. The result is unused — there is no remedy inside a handler.
        unsafe {
            libc::tcsetattr(
                libc::STDIN_FILENO,
                libc::TCSANOW,
                (*RESTORE.saved.get()).as_ptr(),
            );
        }
    }
    // SAFETY: `sigaction` reads a single owned, zeroed `sigaction` through the
    // pointer — `SIG_DFL` needs no mask and no flags — and writes nothing
    // through the null `oldact`; `raise` sends `sig` to this process. Both are
    // async-signal-safe, and both results are unused for the reason above.
    unsafe {
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(sig, &raw const dfl, std::ptr::null_mut());
        libc::raise(sig);
    }
}

/// Install [`restore_and_reraise`] for [`RESTORE_SIGNALS`], once.
///
/// Called by both guards' `arm`, so a session that never changes the terminal
/// never installs a handler at all and a piped run's signal behaviour stays
/// byte-identical to today's (BR-1, AC-7). Idempotent through a [`Once`],
/// which also keeps the disposition readback in
/// `the_handler_re_raises_after_restoring` meaningful.
///
/// **`SA_RESETHAND` is deliberately unset.** With it the handler would fire
/// exactly once, and every later delivery would take the default action with
/// the slot possibly still armed — harmless only because the first delivery
/// kills the process. Leaving the handler installed and resetting the
/// disposition *inside* it makes one path instead of two: every delivery does
/// the same three things, and a second Ctrl-C at the prompt behaves exactly
/// like the first.
///
/// **A disposition of `SIG_IGN` is left alone** (REQ-622, verify). A parent that
/// starts this process with a signal ignored is making a decision *about this
/// process* that the POSIX `exec` contract carries across deliberately — `nohup`
/// and every daemon supervisor rely on it — and installing a handler over it
/// would take that decision away: an ignored `SIGHUP` would start killing a
/// session that was meant to survive its terminal closing, because this handler
/// ends by resetting the disposition to `SIG_DFL` and re-raising. The old
/// disposition is therefore read first, with a null `act`, which is `sigaction`'s
/// own way of asking. The cost of leaving one alone is nothing: a signal that is
/// ignored never ends the process, so there is no exit for the terminal to be
/// left changed across.
fn install_restore_handlers() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for sig in RESTORE_SIGNALS {
            // SAFETY: both calls read and write single owned `sigaction`
            // structs through pointers and touch nothing else; the handler
            // installed is a plain `extern "C"` item in this file. The results
            // are deliberately unused: a process that cannot install the
            // handler still runs, one residual worse than one that can, and
            // there is nothing useful to say about it at the moment a key
            // prompt or a turn is starting.
            unsafe {
                // Ask first. A null `act` makes this a pure read of the
                // current disposition, which is the only way to learn that a
                // parent ignored this signal — and a read that failed leaves
                // `previous` zeroed, whose `sa_sigaction` is `SIG_DFL`, so the
                // fallback is to install, which is this function's job.
                let mut previous: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &raw mut previous);
                if previous.sa_sigaction == libc::SIG_IGN {
                    continue;
                }

                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction =
                    restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&raw mut action.sa_mask);
                // No `SA_RESETHAND` (above) and no `SA_RESTART`: the flags this
                // handler needs are none.
                action.sa_flags = 0;
                libc::sigaction(sig, &raw const action, std::ptr::null_mut());
            }
        }
    });
}

// ---- Test-only items ------------------------------------------------------
//
// Everything below is `#[cfg(test)]`, and the boundary is load-bearing: the
// region sweeps in this file cut their corpus at the **first** top-level
// `#[cfg(test)]` (`production_source`, `handler_body`), so a test-only item
// placed above this line would silently shorten the production text those
// sweeps read and let them pass over code they never saw.

/// Serialises the tests that touch the restore slot.
///
/// The slot is one process-wide global and `cargo test` runs tests in parallel
/// threads, so without this two of them would reach for it at once and see each
/// other's arming — a flake that would look exactly like the bug the slot's own
/// refusal exists to catch. Production has no such lock and must not have one: a
/// signal handler may not take a `Mutex` (see [`RestoreSlot`]), which is the
/// whole reason the slot is an atomic and a cell.
#[cfg(test)]
static SLOT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the slot for the length of a test, poisoned or not — a panic in one of
/// these tests must fail that test, not cascade into the others.
///
/// **`pub(crate)`, and lifted out of this file's own `mod tests`** (REQ-622,
/// verify). Every test that engages a guard needs it, and so does every test
/// *outside* this file that reads [`RawMode::is_engaged`] — that answer is a
/// process-wide global, so a test asserting "raw mode is not engaged" is
/// asserting something another thread's fixture can falsify between the two
/// instructions. `main.rs`'s queued-line test drives `next_interactive_line`,
/// which asks exactly that question, and was order-dependent until it took this.
#[cfg(test)]
pub(crate) fn lock_the_slot() -> std::sync::MutexGuard<'static, ()> {
    SLOT_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// How many times `discard_type_ahead` has been called on this thread.
//
// A tally rather than a terminal, for the reason written at the function: the
// call this test hook stands in for would flush the developer's own shell.
#[cfg(test)]
thread_local! {
    static TYPE_AHEAD_FLUSHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The tally above, readable from `client.rs`'s tests — the caller of
/// [`discard_type_ahead`] is [`crate::client`]'s one question seam, and that is
/// where the rule it serves is asserted.
#[cfg(test)]
pub(crate) fn type_ahead_flushes() -> usize {
    TYPE_AHEAD_FLUSHES.with(std::cell::Cell::get)
}

/// A prompter that replays a fixed list of answers, then returns `None`
/// (simulating EOF). Test-only.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ScriptedPrompter {
    answers: std::collections::VecDeque<String>,
    /// How many times [`ask`](Prompter::ask) was actually called — lets a test
    /// prove an auto-decision consumed no prompt.
    pub asked: usize,
    /// Every question put to the user, in order.
    ///
    /// The question is as user-facing as the answer: it is where a prompt
    /// advertises which keys mean something, and a prompter that dropped it left
    /// that wording assertable only through an e2e (REQ-563 BR-4 — the
    /// persistent key must be offered on exactly the prompts that honour it).
    pub questions: Vec<String>,
    /// The questions asked through [`Prompter::ask_secret`] rather than
    /// [`Prompter::ask`] (REQ-572 AC-5).
    ///
    /// A scripted prompter has no terminal and therefore no echo to switch off,
    /// so the *only* thing a unit test can check is that the flow reached for the
    /// hiding path at all — which is exactly the thing that would silently
    /// regress if a later edit swapped the call back to `ask`. The pty
    /// assertion that the bytes really do not appear is TASK-133's.
    pub secrets: Vec<String>,
}

#[cfg(test)]
impl ScriptedPrompter {
    /// Builds a prompter that will hand back `answers` in order.
    pub fn new(answers: &[&str]) -> Self {
        Self {
            answers: answers.iter().map(|s| (*s).to_owned()).collect(),
            asked: 0,
            questions: Vec::new(),
            secrets: Vec::new(),
        }
    }

    /// Whether any question asked so far contained `needle`.
    pub fn any_question_contains(&self, needle: &str) -> bool {
        self.questions.iter().any(|q| q.contains(needle))
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn ask(&mut self, question: &str) -> Option<String> {
        self.asked += 1;
        self.questions.push(question.to_owned());
        self.answers.pop_front()
    }

    /// The same scripted answer, from the same queue — a test's answers are a
    /// sequence of what the user typed, and which prompt hid the typing does not
    /// change what was typed. What is recorded is *that* this question went
    /// through the hiding path.
    fn ask_secret(&mut self, question: &str) -> Option<String> {
        self.secrets.push(question.to_owned());
        self.ask(question)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- REQ-560 BR-11: the frame's geometry, with no terminal ------------
    //
    // Asserted against the bytes `draw` and `read_line` would write, which is
    // what `draw_bytes`/`advance_bytes` exist to expose. A real terminal is
    // AC-10's job; what is checked here is that the two counts stay a matched
    // pair, because a mismatch is exactly what strands a row.

    /// The one spelling of yes every default-no confirmation in this crate
    /// reads (LESSON-470), now that there is only one of it.
    ///
    /// The negative half is the load-bearing one: empty is the Enter a user
    /// presses to get out of a question, and every other word — including the
    /// ones that read as agreement — is a no, because the flows this gates
    /// spend money, write config, or send.
    #[test]
    fn only_an_explicit_yes_is_a_yes() {
        for yes in ["y", "Y", "yes", "YES", " y ", "Yes\n"] {
            assert!(is_yes(yes), "{yes:?} must consent");
        }
        for no in ["", " ", "n", "N", "no", "sure", "ok", "yes please", "yep"] {
            assert!(!is_yes(no), "{no:?} must not consent");
        }
    }

    /// Without a status row the frame is byte-identical to the pre-REQ-560 one.
    ///
    /// This is the assertion that keeps the change to the *un*-configured case
    /// zero — including the non-interactive path, where `framed` is false and
    /// nothing is written at all (BR-9).
    #[test]
    fn a_frame_with_no_status_row_is_the_frame_it_always_was() {
        let mut p = FramedStdinPrompter::new(true, false);
        let bytes = p.draw_bytes("> ", "");
        assert!(
            bytes.ends_with("\x1b[2A> "),
            "the cursor must rise two rows into the input row: {bytes:?}"
        );
        assert_eq!(p.below_rows, 0);
        // Enter: one newline. EOF: two. Exactly as before.
        assert_eq!(p.advance_bytes(false), "\n");
        assert_eq!(p.advance_bytes(true), "\n\n");
    }

    /// With a status row the frame is four rows, and every count moves together.
    #[test]
    fn a_status_row_adds_one_row_below_and_one_to_every_matching_count() {
        let mut p = FramedStdinPrompter::new(true, false);
        p.set_status(Some("permissions: guarded".to_owned()));
        let bytes = p.draw_bytes("> ", "");

        assert!(
            bytes.contains("permissions: guarded\n"),
            "the status row must be drawn: {bytes:?}"
        );
        assert!(
            bytes.ends_with("\x1b[3A> "),
            "a below-row means the cursor rises one further: {bytes:?}"
        );
        assert_eq!(p.below_rows, 1);

        // The stranding hazard: after Enter the cursor is on the bottom rule, so
        // output resuming one newline later would land *on* the status row.
        assert_eq!(p.advance_bytes(false), "\n\n");
        assert_eq!(p.advance_bytes(true), "\n\n\n");
    }

    /// The status row sits **below** the bottom rule, not above the top one —
    /// which is what makes it independent of REQ-556's above-frame count.
    #[test]
    fn the_status_row_is_drawn_below_the_bottom_rule() {
        let mut p = FramedStdinPrompter::new(true, false);
        p.set_status(Some("permissions: plan".to_owned()));
        let bytes = p.draw_bytes("> ", "");
        let rows: Vec<&str> = bytes.split('\n').collect();
        // [top rule][blank input row][bottom rule][status row][cursor escape…]
        assert_eq!(rows.len(), 5, "the frame should be four rows: {rows:?}");
        assert_eq!(rows[1], "", "the input row is drawn blank");
        assert_eq!(rows[0], rows[2], "the two rules must match");
        assert_eq!(rows[3], "permissions: plan");
    }

    /// Clearing the status row returns the frame to three rows and the counts
    /// with it — the field is what `read_line` reads, so a stale `below_rows`
    /// would step over a row that is no longer there.
    #[test]
    fn clearing_the_status_row_restores_every_count() {
        let mut p = FramedStdinPrompter::new(true, false);
        p.set_status(Some("permissions: full".to_owned()));
        let _ = p.draw_bytes("> ", "");
        assert_eq!(p.below_rows, 1);

        p.set_status(None);
        let bytes = p.draw_bytes("> ", "");
        assert!(bytes.ends_with("\x1b[2A> "), "{bytes:?}");
        assert_eq!(p.below_rows, 0);
        assert_eq!(p.advance_bytes(false), "\n");
    }

    /// REQ-560 BR-9: with the frame off, nothing is drawn and no status byte is
    /// produced — whatever the status is set to.
    #[test]
    fn an_unframed_prompter_emits_no_status_bytes() {
        let mut p = FramedStdinPrompter::new(false, false);
        p.set_status(Some("permissions: full".to_owned()));
        // `draw` returns before composing anything.
        p.draw("> ");
        assert_eq!(
            p.below_rows, 0,
            "an unframed prompter must not accrue rows to step over"
        );
    }

    /// REQ-572 AC-5, fail **closed**: a terminal whose echo could not be
    /// switched off is refused, not read through.
    ///
    /// The dangerous state is the one in the middle. `isatty` passing and
    /// `tcgetattr`/`tcsetattr` failing used to fall into the same `None` as "this
    /// is a pipe", and the read went ahead — at a real terminal, with a real
    /// person typing a real key, and every character painted back. The two
    /// `None`s were the bug: one of them means there is no screen to leak to and
    /// the other means there is one and the guard failed.
    #[test]
    fn a_terminal_that_will_not_hide_the_typing_is_refused_rather_than_read() {
        // A pipe: nothing to switch off, and the read is fine unhidden.
        assert_eq!(classify_echo(false, false, false), EchoOutcome::NoTerminal);
        // The flags a non-tty cannot produce are still a non-tty.
        assert_eq!(classify_echo(false, true, true), EchoOutcome::NoTerminal);
        // A terminal with echo cleared: the happy path.
        assert_eq!(classify_echo(true, true, true), EchoOutcome::Hidden);
        // A terminal whose settings could not be read, or could not be written.
        assert_eq!(classify_echo(true, false, false), EchoOutcome::Failed);
        assert_eq!(classify_echo(true, true, false), EchoOutcome::Failed);

        // And the refusal says the two things a refused prompt has to: that
        // nothing was read, and what to do about it.
        assert!(ECHO_UNAVAILABLE.contains("nothing was read"));
        assert!(ECHO_UNAVAILABLE.contains("stty sane"));
    }

    /// REQ-573: a question is defused before it reaches the terminal, and an
    /// ordinary one is not touched.
    ///
    /// The hazard is not hypothetical: since the `/web setup` catalog became the
    /// daemon's (REQ-573), the auth-header template named in the prompt is an
    /// RPC-supplied string, and the permission flow's `{tool}` prompt has carried
    /// daemon-supplied text since it shipped. Both land here, which is the one
    /// writer to this terminal that is not a `Surface` — so the transform has to
    /// be *this* function rather than a second one that drifts.
    ///
    /// Asserted through `draw_bytes` because that is the composition a test can
    /// read without a terminal; `StdinPrompter::ask`/`ask_secret` write the
    /// result of the same `defused` call, and `FramedStdinPrompter` with the
    /// frame off delegates straight to them.
    #[test]
    fn a_question_is_defused_before_it_reaches_the_terminal() {
        let mut p = FramedStdinPrompter::new(true, false);

        // The repaint attack, aimed at a prompt instead of at a rendered line:
        // erase this row, step up, and rewrite the question the user already
        // read. Every commanding byte becomes a space, so the text stays visible
        // and stays inert.
        let hostile = "  auth header template [Enter for `\x1b[2KX-Evil: {key}\rgotcha`]: ";
        let expected = crate::render::defused(hostile);
        let bytes = p.draw_bytes(hostile, "");
        let (frame, question) = bytes.split_at(bytes.len() - expected.len());
        assert_eq!(question, expected);
        assert!(
            frame.ends_with("\x1b[2A"),
            "the frame's own cursor escape is the only one left: {frame:?}"
        );
        assert!(
            !question.contains('\x1b') && !question.contains('\r'),
            "no byte that commands the terminal may survive: {question:?}"
        );
        assert!(
            question.contains("X-Evil: {key}") && question.contains("gotcha"),
            "the text itself is still shown — defusing is not hiding: {question:?}"
        );

        // And the questions this binary actually composes are byte-identical, so
        // the guard costs the ordinary path nothing.
        for ordinary in [
            "  tier [1-3, or Enter to cancel]: ",
            "  auth header template [Enter for `Authorization: Bearer {key}`]: ",
            "> ",
        ] {
            let bytes = p.draw_bytes(ordinary, "");
            assert!(
                bytes.ends_with(ordinary),
                "an ordinary question must survive verbatim: {bytes:?}"
            );
        }
    }

    /// **The chevron's tint belongs to the seam, not the caller (REQ-573).**
    ///
    /// The defusing fix briefly shipped its own regression: `main.rs` used to
    /// hand the entry prompt in with SGR already composed, which the sanitizer
    /// then (correctly) shredded into literal `[36m` debris. The tint is now
    /// applied here, after defusing, so a plain-text question arrives styled
    /// and a hostile question still arrives dead.
    #[test]
    fn the_chevrons_tint_is_applied_after_defusing_at_the_seam() {
        // Colour on: plain " › " in, tinted chevron out.
        let mut tinted = FramedStdinPrompter::new(true, true);
        let bytes = tinted.draw_bytes(" › ", "");
        assert!(
            bytes.contains(" \x1b[36m›\x1b[0m "),
            "the seam must tint the chevron the caller handed in plain: {bytes:?}"
        );

        // Colour off: the same question stays plain, and no SGR appears.
        let mut plain = FramedStdinPrompter::new(true, false);
        let bytes = plain.draw_bytes(" › ", "");
        assert!(
            bytes.contains(" › ") && !bytes.contains("\x1b[36m"),
            "no colour means no SGR at all: {bytes:?}"
        );

        // A hostile question is defused whether or not the tint applies — the
        // erase and the carriage return die, the chevron still gets its tint.
        let mut hostile = FramedStdinPrompter::new(true, true);
        let bytes = hostile.draw_bytes("\x1b[2K› \r", "");
        assert!(
            !bytes.contains("\x1b[2K") && !bytes.contains('\r'),
            "defusing must run regardless of styling: {bytes:?}"
        );
        assert!(
            bytes.contains("\x1b[36m›\x1b[0m"),
            "styling must still apply to the defused text: {bytes:?}"
        );
    }

    #[test]
    fn scripted_prompter_replays_then_reports_eof() {
        let mut p = ScriptedPrompter::new(&["y", "n"]);
        assert_eq!(p.ask("q1"), Some("y".to_owned()));
        assert_eq!(p.ask("q2"), Some("n".to_owned()));
        assert_eq!(p.ask("q3"), None);
        assert_eq!(p.asked, 3);
    }

    /// REQ-572 AC-5's unit-testable half: a secret question is answered from the
    /// same queue and is recorded as having taken the hiding path, so a flow that
    /// asked for a credential through the echoing `ask` fails here rather than in
    /// a pty capture nobody reads.
    #[test]
    fn a_scripted_secret_answers_from_the_same_queue_and_is_recorded_separately() {
        let mut p = ScriptedPrompter::new(&["plain", "sk-secret"]);
        assert_eq!(p.ask("endpoint? "), Some("plain".to_owned()));
        assert_eq!(p.ask_secret("api key: "), Some("sk-secret".to_owned()));
        assert_eq!(p.secrets, vec!["api key: ".to_owned()]);
        assert_eq!(p.questions.len(), 2, "both questions are still recorded");
        assert_eq!(p.asked, 2, "a secret read is still a read");
    }

    // ---- REQ-622: the terminal's mode, and the undo a signal can find ------
    //
    // Asserted without depending on a terminal, on purpose. Every `tcsetattr`
    // a unit test can cause is aimed at descriptor 0, which under `cargo test`
    // from a terminal is the developer's own — so the flag arithmetic goes
    // through `raw_from`, the classification through `classify_raw`, and the
    // bookkeeping through `arm` with the settings that are already in effect.
    // `engage` itself is exercised too, and its three outcomes are all
    // legitimate here, so the same assertions hold over a pipe and at a
    // terminal. No signal is delivered: the real delivery, the child's exit
    // status and the terminal the process leaves behind are TASK-419's pty
    // legs.

    /// The terminal's settings as they are right now, or a zeroed struct when
    /// descriptor 0 is not a terminal.
    ///
    /// Handing a guard the settings that are *already in effect* makes the
    /// `tcsetattr` in its `Drop` a no-op at a real terminal and an `ENOTTY`
    /// no-op everywhere else, so the slot bookkeeping is assertable without a
    /// pty and without a chance of leaving somebody's shell in a state `stty
    /// sane` has to fix.
    fn current_termios() -> libc::termios {
        // SAFETY: `tcgetattr` writes a single owned `termios` through the
        // pointer and touches nothing else; a failure leaves the zeroed
        // struct, which is what a non-terminal descriptor wants — `tcsetattr`
        // will refuse it with `ENOTTY` exactly as it refuses anything else.
        unsafe {
            let mut current: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &raw mut current);
            current
        }
    }

    /// The body of [`restore_and_reraise`], read out of this file's own bytes.
    ///
    /// A claim about what a function does *not* contain cannot be asserted by
    /// running it, and this particular function cannot be run at all without
    /// ending the process — that is its job. So it is asserted about the
    /// source, embedded with `include_str!` at compile time rather than read
    /// from disk: BUG-159's trap is a scan that opens files at runtime and
    /// passes vacuously from a directory that is not the crate.
    ///
    /// Bounded to the item, per REQ-600: the corpus is cut at the first
    /// column-0 `#[cfg(test)]` so the needles written in this module cannot
    /// match themselves, and the span then runs from the handler's signature
    /// to the column-0 brace that closes it — so the claim is about this
    /// function's body and not about the rest of the file.
    fn handler_body() -> &'static str {
        const SOURCE: &str = include_str!("prompt.rs");
        let production = SOURCE
            .split_once("\n#[cfg(test)]")
            .map_or(SOURCE, |(before, _)| before);
        let start = production
            .find("extern \"C\" fn restore_and_reraise(")
            .expect("the restore handler must be in this file");
        let body = &production[start..];
        let end = body
            .find("\n}\n")
            .expect("the restore handler's body must be closed by a column-0 brace");
        &body[..end]
    }

    /// REQ-622 BR-1: `ICANON` and `ECHO` go, `VMIN`/`VTIME` become zero, and
    /// nothing else in any of the four flag words moves.
    ///
    /// The load-bearing half is the negative one, and it is not decoration.
    /// `ISIG` surviving is what makes BR-8 true by construction — clear it and
    /// Ctrl-C stops being a signal and starts being a `0x03` byte in the
    /// editor's buffer, with the restore path never running at all. `ICRNL` and
    /// `OPOST` surviving is what keeps every byte assertion in the pty suite
    /// true: clearing `OPOST` turns each `\n` a `Surface` writes into a bare
    /// line feed and shifts every row one column right, for ever.
    ///
    /// Asserted against a synthetic "as found" termios so the arithmetic is
    /// visible with no terminal in the room, and the expected `c_lflag` is
    /// enumerated by name rather than recomputed with the subject's own
    /// expression (LESSON-569: never let the oracle be the code under test).
    #[test]
    fn raw_mode_changes_only_icanon_and_echo() {
        // SAFETY: `zeroed` produces a plain POD struct with no invariants; the
        // fields set below are the ones this test reads back, and it is never
        // handed to a syscall.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        saved.c_iflag = libc::ICRNL | libc::IXON | libc::BRKINT;
        saved.c_oflag = libc::OPOST | libc::ONLCR;
        saved.c_cflag = libc::CS8 | libc::CREAD;
        saved.c_lflag =
            libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHONL | libc::ISIG | libc::IEXTEN;
        saved.c_cc[libc::VMIN] = 1;
        saved.c_cc[libc::VTIME] = 7;
        saved.c_cc[libc::VINTR] = 3;

        let raw = raw_from(saved);

        // The two flags that change, and the exact remainder of the word.
        assert_eq!(
            raw.c_lflag & libc::ICANON,
            0,
            "the kernel must stop assembling lines"
        );
        assert_eq!(
            raw.c_lflag & libc::ECHO,
            0,
            "the kernel must stop painting keystrokes"
        );
        assert_eq!(
            raw.c_lflag,
            libc::ECHOE | libc::ECHONL | libc::ISIG | libc::IEXTEN,
            "every other local flag must survive verbatim: {:#x}",
            raw.c_lflag
        );
        assert_ne!(
            raw.c_lflag & libc::ISIG,
            0,
            "Ctrl-C must still be a SIGINT (BR-8)"
        );

        // The other three words are not touched at all.
        assert_eq!(raw.c_iflag, saved.c_iflag, "the input flags must not move");
        assert_eq!(raw.c_oflag, saved.c_oflag, "the output flags must not move");
        assert_eq!(
            raw.c_cflag, saved.c_cflag,
            "the control flags must not move"
        );
        assert_ne!(
            raw.c_iflag & libc::ICRNL,
            0,
            "Enter must still arrive as a newline"
        );
        assert_ne!(
            raw.c_oflag & libc::OPOST,
            0,
            "every row a Surface writes must still end the way it does today"
        );

        // A read returns immediately with whatever is there.
        assert_eq!(raw.c_cc[libc::VMIN], 0);
        assert_eq!(raw.c_cc[libc::VTIME], 0);
        // And no other control character moved — the interrupt character above
        // all, since it is the other half of `ISIG`.
        for (i, (before, after)) in saved.c_cc.iter().zip(raw.c_cc.iter()).enumerate() {
            if i == libc::VMIN || i == libc::VTIME {
                continue;
            }
            assert_eq!(before, after, "control character {i} must not move");
        }
        assert_eq!(
            raw.c_cc[libc::VINTR],
            3,
            "the interrupt character must not move"
        );
    }

    /// REQ-622 BR-7: both guards arm the one restore slot, and both `Drop`s
    /// clear it.
    ///
    /// This is the bookkeeping the signal handler reads, so it is the
    /// bookkeeping that decides whether a Ctrl-C restores. It is exercised
    /// through `arm` — the function `engage` calls on its success path — with
    /// the terminal's *current* settings, so the `tcsetattr` each `Drop`
    /// performs is a no-op; calling `engage` itself would aim a real mode
    /// change at descriptor 0, and TASK-419's pty leg is where that belongs.
    ///
    /// # What breaks this test
    ///
    /// | Mutation | Fails |
    /// |---|---|
    /// | `RESTORE.clear()` is removed from `RawMode`'s `Drop` | **4 red of 872** in this binary and **8 red of 54** in `pty_e2e` (re-run 2026-09-10 at the verify-fix pass) |
    ///
    /// This test is the intended red, on `` `RawMode`'s `Drop` must clear the
    /// slot `` (over a pipe) or on the earlier "dropping it must leave the slot
    /// empty" (on a pty, where the real `engage` arms first) — which assertion
    /// fires depends on what descriptor 0 is, and both are this test's.
    ///
    /// **The other reds are the finding, and they were not in the first reading
    /// of this mutation.** Recorded at TASK-416 as "this test and nothing else,
    /// 1 of 835", which was true of a suite in which nothing yet read the slot.
    /// It is now read for a second purpose: a slot that is never disarmed leaves
    /// [`RawMode::is_engaged`] permanently `true` for the rest of the process,
    /// and `main.rs`'s `queued_for_entry` consults it to decide whether the
    /// entry frame may drain the queue. So every line the editor took during a
    /// turn is **stranded** — never sent, never shown. That is the single cause
    /// of all eight pty reds
    /// (`a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt`,
    /// `queued_lines_become_the_next_prompts_in_order`,
    /// `a_question_never_eats_type_ahead`, `a_queued_line_is_announced_on_the_row`,
    /// `the_queued_hint_moves_to_the_pending_row_while_the_reply_streams`,
    /// `a_pasted_block_queues_one_prompt_per_line`, `unhandled_keys_are_inert`
    /// and `multi_byte_input_round_trips`), and of the unit red
    /// `main::tests::queued_lines_re_enter_ahead_of_the_poll_in_order`. That one
    /// is **collateral and order-dependent**: it passes when run alone and fails
    /// in a full-suite run, because the slot is process-wide and it does not
    /// take [`lock_the_slot`]. It is named here rather than counted as evidence.
    ///
    /// Two figures moved at the verify-fix pass, and both are recorded rather
    /// than quietly restated. The unit count went from two to **four** with
    /// `a53e9a6`'s own slot cases — `the_slot_refuses_a_second_guard_and_only_
    /// its_owner_clears_it` and `an_engage_against_a_held_slot_fails_rather_
    /// than_changing_the_terminal` both read the arm this mutation leaves set.
    /// And the two pty legs this record used to name as *accidental* reds —
    /// `a_turn_boundary_closes_an_unclosed_fence_at_a_terminal` and
    /// `a_resized_window_lays_the_next_turn_out_at_the_new_width`, each caught
    /// because its second prompt is typed while the first turn is still closing
    /// out — **did not reproduce**. They were timing, as the word "accident"
    /// said, and they are dropped from the list rather than carried as coverage
    /// nothing can rely on (LESSON-441).
    ///
    /// The property this test owns is still its own: the bookkeeping is what the
    /// signal handler reads, so a guard that stops disarming leaves the handler
    /// ready to write settings the process no longer owns over a terminal
    /// somebody else has since changed — and no pty leg above asks that, because
    /// they all fail on the queue first.
    #[test]
    fn both_guards_arm_and_clear_the_restore_slot() {
        let _slot = lock_the_slot();
        let saved = current_termios();

        assert_eq!(
            RESTORE.armed_by(),
            SLOT_EMPTY,
            "the slot starts owing nothing"
        );
        assert!(!RawMode::is_engaged());

        // First the real `engage`s, whatever descriptor 0 happens to be. All
        // three outcomes are legitimate — a terminal under `cargo test` from a
        // shell, a pipe under CI — and one invariant covers all three: the slot
        // is armed exactly when a guard exists, and the outcome says which. A
        // real terminal does go raw for the length of these few lines, and that
        // is safe by the mechanism under test: `Drop` restores it, and a signal
        // inside the window restores it too.
        match RawMode::engage() {
            RawOutcome::Raw(guard) => {
                assert_eq!(
                    RESTORE.armed_by(),
                    SLOT_RAW,
                    "a real engage must arm the slot"
                );
                assert!(RawMode::is_engaged());
                // Deliberately no comparison against `saved` here. macOS sets
                // `PENDIN` in `c_lflag` on the way out of non-canonical mode —
                // a driver status bit, not a setting — so two `tcgetattr`s
                // either side of a mode cycle differ by a bit no code in this
                // file wrote. Whether the slot holds what it was handed is the
                // deterministic leg's claim below, where the value is
                // synthetic; here the claim is only that a real engage arms.
                drop(guard);
            }
            // No terminal, or a terminal that refused. Nothing was changed, so
            // nothing is owed: BR-11's fail-open leaves no bookkeeping behind.
            RawOutcome::NoTerminal | RawOutcome::Failed => {
                assert_eq!(RESTORE.armed_by(), SLOT_EMPTY);
            }
        }
        assert_eq!(
            RESTORE.armed_by(),
            SLOT_EMPTY,
            "whatever `engage` returned, dropping it must leave the slot empty"
        );

        match EchoOff::engage() {
            EchoState::Hidden(guard) => {
                assert_eq!(
                    RESTORE.armed_by(),
                    SLOT_ECHO_OFF,
                    "the key prompt arms the same slot"
                );
                assert!(!RawMode::is_engaged());
                drop(guard);
            }
            EchoState::NoTerminal | EchoState::Failed => {
                assert_eq!(RESTORE.armed_by(), SLOT_EMPTY);
            }
        }
        assert_eq!(RESTORE.armed_by(), SLOT_EMPTY);

        // Then the deterministic half, through `arm` — the function `engage`
        // calls on its success path — so the arming and the clearing are
        // asserted on every machine and not only on the ones with a terminal.
        let guard = RawMode::arm(saved).expect("an empty slot takes the first guard");
        assert_eq!(RESTORE.armed_by(), SLOT_RAW);
        assert!(
            RESTORE.holds(&saved),
            "the settings a signal would put back must be the ones the guard saved"
        );
        assert!(
            RawMode::is_engaged(),
            "the prompter reads raw mode off the slot, not off a threaded flag (ADR-622-2)"
        );
        drop(guard);
        assert_eq!(
            RESTORE.armed_by(),
            SLOT_EMPTY,
            "`RawMode`'s `Drop` must clear the slot"
        );
        assert!(!RawMode::is_engaged());

        // REQ-572's accepted residual, retired: the key prompt's guard
        // registers in the same slot, so the same handler covers it (AC-6).
        let guard = EchoOff::arm(saved).expect("an empty slot takes the first guard");
        assert_eq!(RESTORE.armed_by(), SLOT_ECHO_OFF);
        assert!(RESTORE.holds(&saved));
        assert!(
            !RawMode::is_engaged(),
            "echo off is canonical mode with the painting switched off, not raw mode"
        );
        drop(guard);
        assert_eq!(
            RESTORE.armed_by(),
            SLOT_EMPTY,
            "`EchoOff`'s `Drop` must clear the slot"
        );
    }

    /// **REQ-622, verify: the slot refuses a second guard in *every* build, and
    /// a refused guard never disarms the one that holds it.**
    ///
    /// ADR-622-3 says two guards are never engaged at once, and that is true of
    /// today's control flow. It was enforced by a `debug_assert`, which means a
    /// release build did the one thing nobody would choose: overwrite the first
    /// guard's saved settings with the second's. The outer guard's `Drop` would
    /// then write back settings that were *already changed* — the user's
    /// terminal left in the inner guard's mode, with nothing in the process that
    /// knows how to undo it, which is the exact residual this whole mechanism
    /// exists to close. A reachability argument is a reason not to expect a
    /// case; it is never a reason to make it destructive.
    ///
    /// Driven through `arm`/`store` with synthetic settings rather than through
    /// `engage`, so it holds on a machine with no terminal and cannot leave the
    /// developer's shell raw.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** put the refusal back
    /// as a `debug_assert_eq!` and let the store proceed. **1 red of 872**, this
    /// test — it panics on the `debug_assert` under `cargo test`, which is the
    /// honest report that a release build would have carried on and clobbered
    /// the slot. Reverted with the same edit.
    #[test]
    fn the_slot_refuses_a_second_guard_and_only_its_owner_clears_it() {
        let _slot = lock_the_slot();
        let mut first = current_termios();
        // A value the other guard's settings cannot be confused with. Which
        // flag does not matter — what matters is that `holds` can tell the two
        // apart, so "the slot still holds the first guard's settings" is a claim
        // about the bytes and not about a word.
        first.c_lflag |= libc::ICANON;
        let mut second = first;
        second.c_lflag &= !libc::ICANON;

        assert_eq!(RESTORE.armed_by(), SLOT_EMPTY, "the slot starts empty");
        let held = RawMode::arm(first).expect("an empty slot takes the first guard");
        assert_eq!(RESTORE.armed_by(), SLOT_RAW);
        assert!(RESTORE.holds(&first));

        // The second guard is refused, and refused **without touching the
        // slot** — which is the half that matters, since the first guard's
        // `Drop` is about to write whatever is in there back to the terminal.
        assert!(
            !RESTORE.store(&second, SLOT_ECHO_OFF),
            "a slot another guard holds must refuse, not overwrite"
        );
        assert_eq!(RESTORE.armed_by(), SLOT_RAW);
        assert!(
            RESTORE.holds(&first),
            "and the settings in it are still the holder's"
        );
        assert!(
            EchoOff::arm(second).is_none(),
            "so `arm` hands back no guard"
        );
        assert_eq!(RESTORE.armed_by(), SLOT_RAW);

        // And a guard that was refused must not disarm the one that holds it: a
        // `Drop` order away from costing the holder its restore entirely.
        RESTORE.clear(SLOT_ECHO_OFF);
        assert_eq!(
            RESTORE.armed_by(),
            SLOT_RAW,
            "only the owner clears the slot"
        );

        drop(held);
        assert_eq!(RESTORE.armed_by(), SLOT_EMPTY, "the owner's `Drop` does");
    }

    /// **REQ-622, verify: with the slot already held, an engage fails rather
    /// than changing a terminal nothing can put back.**
    ///
    /// The refusal above, mapped by each caller to the outcome it already has
    /// for "this terminal would not take the change" — and the two polarities
    /// are the point. `RawMode` fails **open**: the turn runs in canonical mode
    /// with one verbose notice (BR-11). `EchoOff` fails **closed**: the
    /// credential is not read at all, because reading it would paint it into the
    /// user's scrollback.
    ///
    /// The `tcsetattr` is what must not happen, and `engage` reads the arm's
    /// answer before it: a mode change nothing can undo is worse than no mode
    /// change.
    #[test]
    fn an_engage_against_a_held_slot_fails_rather_than_changing_the_terminal() {
        let _slot = lock_the_slot();
        let saved = current_termios();
        let held = RawMode::arm(saved).expect("an empty slot takes the first guard");

        assert!(
            matches!(
                RawMode::engage(),
                RawOutcome::NoTerminal | RawOutcome::Failed
            ),
            "a second raw engage never reports `Raw` against a held slot — and \
             on a machine with no terminal it never gets that far either"
        );
        assert!(
            matches!(EchoOff::engage(), EchoState::NoTerminal | EchoState::Failed),
            "and the key prompt's guard is refused the same way, where its own \
             caller turns the refusal into a declined read"
        );
        assert_eq!(
            RESTORE.armed_by(),
            SLOT_RAW,
            "neither attempt disturbed the slot the first guard holds"
        );
        drop(held);
        assert_eq!(RESTORE.armed_by(), SLOT_EMPTY);
    }

    /// **REQ-622, verify: a credential is never read while the terminal is
    /// raw.**
    ///
    /// Unreachable today — a key prompt is only opened between turns, where
    /// nothing is engaged — and cheap insurance against the day a setup flow is
    /// reached from inside one. The failure it forecloses is not subtle: this
    /// prompter's hidden read is a canonical-mode `read_line`, so with `ICANON`
    /// clear it would sit waiting for a newline the line discipline is no longer
    /// going to synthesize, and the echo guard restoring afterwards would
    /// restore the *raw* settings, because they are what it saved.
    ///
    /// Three legs, because the rule has three parts and only one of them is
    /// reachable through a public function without a terminal. The **bytes** of
    /// the refusal are asserted directly on [`refuse_secret`]; the **check**, at
    /// the top of `ask_secret` and ahead of the engage, is a region assertion,
    /// which is what a rule about *ordering inside a function that reads stdin*
    /// can be pinned by ([[LESSON-547]]); and the **backstop** is the slot's
    /// own refusal, asserted just above.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the
    /// `if RawMode::is_engaged() { return refuse_secret(&mut out); }` from
    /// `StdinPrompter::ask_secret`. **1 red of 872**, this test, on the region
    /// leg. Reverted with the same edit.
    #[test]
    fn a_key_prompt_refuses_to_read_while_the_terminal_is_raw() {
        let mut out = Vec::new();
        assert_eq!(refuse_secret(&mut out), None, "nothing is read");
        let written = String::from_utf8(out).expect("utf-8");
        assert_eq!(
            written,
            format!("\n{ECHO_UNAVAILABLE}\n"),
            "one blank line to leave the question's row, then the notice — the \
             same bytes both refusals write, because the user-facing fact is the \
             same: nothing was read and nothing was stored"
        );

        let source = production_source();
        let at = source
            .find("impl Prompter for StdinPrompter {")
            .expect("the plain prompter");
        // From `ask_secret` onward, and not from the `impl` — `ask` a few lines
        // above asks `RawMode::is_engaged()` too, for the opposite purpose (it
        // *takes* the raw path), so a search across the whole block would find
        // that one and pass with `ask_secret` unguarded.
        let body = &source[at..];
        let secret = body
            .find("fn ask_secret(")
            .expect("the plain prompter asks for credentials");
        let secret_body = &body[secret..];
        let engage = secret_body
            .find("EchoOff::engage()")
            .expect("and hides them with the echo guard");
        let check = secret_body.find("if RawMode::is_engaged() {").expect(
            "a credential must not be read in raw mode: a canonical-mode \
             `read_line` would wait for a newline the line discipline no longer \
             makes, and the echo guard would restore the raw settings it saved",
        );
        assert!(
            check < engage,
            "and the check belongs **before** the engage: a mode change made and \
             then refused is a mode change"
        );
    }

    /// **REQ-622, verify: `read_answer_raw` clamps what its reader reports to
    /// the buffer it handed over.**
    ///
    /// `RowState::read_keys` clamps its own for this reason and this test is the
    /// other half of it: production cannot report more bytes than the slice it
    /// was given, so the clamp is not about production — it is about the seam.
    /// A hook that over-reports panics the slice, and a panic inside a read loop
    /// is a crash rather than a failed assertion, which is a much worse way to
    /// learn that a test double is wrong.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** drop the
    /// `.min(buf.len())`. **1 red of 872**, this test, as a slice-index panic
    /// (`range end index 1024 out of range for slice of length 256`) rather than
    /// an assertion. Reverted with the same edit.
    #[test]
    fn an_over_reporting_reader_cannot_panic_the_answer_loop() {
        let mut answer = InputEditor::default();
        let mut out = Vec::new();
        let mut passes = 0usize;
        let answered = read_answer_raw(
            &mut answer,
            "? proceed: ",
            &mut out,
            |buf| {
                passes += 1;
                if passes == 1 {
                    // A liar: four times the buffer it was handed.
                    buf[..2].copy_from_slice(b"y\n");
                    Ok(buf.len() * 4)
                } else {
                    Ok(0)
                }
            },
            || false,
        );
        assert_eq!(
            answered.as_deref(),
            Some("y"),
            "the answer is read from the bytes that were actually written, and \
             the over-report costs nothing but the bytes it invented"
        );
    }

    /// **REQ-622, verify: a signal a parent set to `SIG_IGN` keeps that
    /// disposition.**
    ///
    /// Ignoring a signal across `exec` is a deliberate part of the POSIX
    /// contract and the mechanism `nohup` and every daemon supervisor are built
    /// on: the parent is making a decision *about this process*. Installing a
    /// handler over it takes that decision away — and takes it away in the worst
    /// direction, because this handler ends by resetting the disposition to
    /// `SIG_DFL` and re-raising, so an ignored `SIGHUP` would start **killing**
    /// a session that was meant to survive its terminal closing. The cost of
    /// leaving one alone is nothing: a signal that is ignored never ends the
    /// process, so there is no exit for the terminal to be left changed across.
    ///
    /// A region assertion rather than a behavioural one, and the reason is the
    /// [`Once`]: the installer runs at most once per process and other tests in
    /// this file have already run it, so there is no second call for a test to
    /// observe. A subprocess with an ignored signal is the pty suite's to own.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the
    /// `if previous.sa_sigaction == libc::SIG_IGN { continue; }` guard. **1 red
    /// of 872**, this test. Reverted with the same edit.
    #[test]
    fn a_signal_a_parent_ignored_is_left_ignored() {
        let source = production_source();
        let at = source
            .find("fn install_restore_handlers() {")
            .expect("the installer");
        let body = &source[at..];
        // Bounded to the function: a `\n}\n` is a closing brace at column 0, and
        // nothing nested reaches column 0.
        let body = &body[..body.find("\n}\n").expect("the installer's brace")];

        let read = body
            .find("libc::sigaction(sig, std::ptr::null(), &raw mut previous)")
            .expect(
                "the installer must read the current disposition first — a null \
                 `act` is `sigaction`'s own way of asking — or it cannot know \
                 that a parent ignored this signal",
            );
        let skip = body
            .find("if previous.sa_sigaction == libc::SIG_IGN {")
            .expect(
                "and must leave an ignored signal alone: this handler resets to \
                 `SIG_DFL` and re-raises, so installing over `SIG_IGN` turns a \
                 signal the parent made harmless into one that kills",
            );
        let install = body
            .find("libc::sigaction(sig, &raw const action, std::ptr::null_mut())")
            .expect("and otherwise installs the handler");
        assert!(
            read < skip && skip < install,
            "read the disposition, decide, then install — in that order: {body}"
        );
        assert!(
            body[skip..install].contains("continue;"),
            "the decision has to *skip* this signal, not merely be computed: {body}"
        );
    }

    /// REQ-622 BR-8: the handler is installed for all three signals, and its
    /// body restores, resets the disposition, and re-raises — in that order and
    /// with nothing else in it.
    ///
    /// **No signal is delivered here, deliberately.** Restoring and re-raising
    /// is what the handler does *instead of returning*, so exercising it in
    /// this process would end the test binary — correctly, with the signal's
    /// own status, which cargo would report as a crash. The delivery, the
    /// child's exit status and the terminal it leaves behind are TASK-419's pty
    /// leg. What a unit can pin is the disposition (read back through
    /// `sigaction`, so the claim is the kernel's answer and not our own
    /// bookkeeping) and the body, and the body is a negative claim about code
    /// that must *not* be there — hence the source scan.
    ///
    /// # What breaks this test (REQ-622 TASK-419)
    ///
    /// The mutation below was **applied, rebuilt and observed failing**, not
    /// reasoned about (LESSON-441, LESSON-568):
    ///
    /// | Mutation | Fails |
    /// |---|---|
    /// | the `tcsetattr` call is removed from [`restore_and_reraise`] | **4 red of 999** (2026-09-10): this test, on "the handler must put the terminal back", plus the three pty legs that end a session with a signal — `ctrl_c_restores_the_terminal`, `every_exit_restores_the_terminal` (its SIGTERM leg) and `the_key_prompt_survives_ctrl_c_with_echo_on`, each on the flags `stty -a` reads back off their pty. `cli_e2e` stays green, all 97, which is AC-7 arriving as evidence rather than as an argument |
    /// | `SIGQUIT` is dropped from [`RESTORE_SIGNALS`] (the state before the verify pass) | **1 red of 872** (2026-09-10): this test, on the length and on the `contains`. Only this one, which is the argument for asserting the *set* and not only the handler: Ctrl-\\ on a raw terminal is one keystroke away from Ctrl-C and there was no leg anywhere that asked about it |
    ///
    /// The pair is the point, and it is why the source scan here is not
    /// redundant with the pty legs. This test fails on the *shape* of the
    /// handler and would fail on a machine with no pty at all; the pty legs
    /// fail on what a user's terminal is actually left in. A handler that
    /// called `tcsetattr` on the wrong descriptor would pass this one and
    /// redden those, and a suite with no terminal in it would have only this.
    #[test]
    fn the_handler_re_raises_after_restoring() {
        install_restore_handlers();

        let ours = restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
        assert_eq!(
            RESTORE_SIGNALS.len(),
            4,
            "BR-7's three, plus the `SIGQUIT` the verify pass added: it is a \
             **keystroke** on the very keyboard this guard has put into raw \
             mode, and its default action kills the process"
        );
        assert!(
            RESTORE_SIGNALS.contains(&libc::SIGQUIT),
            "and it is `SIGQUIT` specifically that was missing"
        );
        for sig in RESTORE_SIGNALS {
            // SAFETY: `sigaction` with a null `act` only reads the current
            // disposition, writing it into a single owned struct through
            // `oldact`; nothing is installed by this call. The zeroed struct
            // it writes into is plain POD.
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &raw mut current) };
            assert_eq!(rc, 0, "signal {sig} must have a readable disposition");
            assert_eq!(
                current.sa_sigaction, ours,
                "signal {sig} must be handled by the restore handler"
            );
            assert_eq!(
                current.sa_flags & libc::SA_RESETHAND,
                0,
                "SA_RESETHAND must stay unset for signal {sig}, so a second Ctrl-C behaves \
                 exactly like the first"
            );
        }

        let handler = handler_body();
        // The span really is the handler and stops at its own brace — without
        // this the needles below would be claims about the rest of the file.
        assert!(
            handler.contains("RESTORE.armed_by()")
                && !handler.contains("fn install_restore_handlers"),
            "the scanned span must be the handler's body and nothing after it: {handler}"
        );

        assert!(
            handler.contains("libc::tcsetattr("),
            "the handler must put the terminal back: {handler}"
        );
        assert!(
            handler.contains("libc::SIG_DFL"),
            "the handler must reset the disposition, or the raise below would re-enter it: \
             {handler}"
        );
        assert!(
            handler.contains("libc::raise(sig)"),
            "the handler must hand the signal back to the kernel, so the process dies of it \
             with its status (BR-8): {handler}"
        );

        // Restore first, then die. The reverse order restores nothing.
        let restore = handler.find("libc::tcsetattr(").expect("asserted above");
        let default = handler.find("libc::SIG_DFL").expect("asserted above");
        let reraise = handler.find("libc::raise(sig)").expect("asserted above");
        assert!(
            restore < default && default < reraise,
            "the handler must restore, then reset, then raise: {handler}"
        );

        // And nothing that a signal handler may not do. Each of these is a
        // real way to write this function wrong: a notice printed on the way
        // out allocates, a `Mutex` round the slot deadlocks against the thread
        // the signal interrupted, and either one hangs or aborts a process the
        // user has just asked to die.
        for forbidden in [
            "format!",
            "println!",
            "eprintln!",
            "write!",
            "String",
            "to_owned",
            "vec!",
            "Box::",
            "Mutex",
            ".lock()",
        ] {
            assert!(
                !handler.contains(forbidden),
                "`{forbidden}` is not async-signal-safe and must not appear in the handler: \
                 {handler}"
            );
        }
    }

    /// REQ-622 ADR-622-1: the pump's read takes what is there and returns.
    ///
    /// The two properties a tick depends on, and both are asserted by the test
    /// completing at all: it never blocks — a `read` that waited for a line
    /// would hang here rather than fail, which is how a missing `VMIN`
    /// announces itself — and it never reports more than the buffer it was
    /// given. The zero-length case is the one that is deterministic
    /// everywhere: no descriptor is touched, so nothing can be consumed by a
    /// buffer with nowhere to put it.
    ///
    /// What bytes actually arrive is the pty leg's (TASK-419); a unit test has
    /// no keystrokes to offer, and asserting that none arrived would be
    /// asserting the environment rather than the product.
    #[test]
    fn read_available_takes_what_is_there_and_never_waits() {
        assert_eq!(
            read_available(&mut []).expect("a zero-length read cannot fail"),
            0,
            "a buffer with nowhere to put a byte must consume none"
        );

        let mut buf = [0u8; 64];
        let read = read_available(&mut buf).expect("a non-blocking read of stdin cannot fail here");
        assert!(
            read <= buf.len(),
            "a read must never report more than the buffer it was handed: {read}"
        );
    }

    /// REQ-622 BR-11: a terminal that refuses the mode change is named, and the
    /// name is not the one a pipe gets.
    ///
    /// The truth table is [`classify_echo`]'s, fact for fact — which is the
    /// point. The fail-**open** polarity is not in this function at all; it is
    /// in what the caller does with `Failed`, and it differs from the key
    /// prompt's fail-closed for one reason: there, failing to hide a credential
    /// means not reading it, and here there is nothing to hide, so the turn
    /// runs in canonical mode.
    ///
    /// **The trap this catches.** The lazy way to fail open is to hand a
    /// refusing terminal the same verdict as a pipe. The caller then carries on
    /// — which is right — and says nothing, which is not: BR-11 asks for
    /// exactly one notice, and only a terminal that refused has earned it. A
    /// pipe must stay silent (AC-7), so the two states have to stay two.
    #[test]
    fn classify_raw_fails_open() {
        // A pipe: there was no canonical mode to leave, and `engage` makes no
        // `tcsetattr` at all (BR-1, AC-7).
        assert_eq!(classify_raw(false, false, false), RawVerdict::NoTerminal);
        // The flags a non-tty cannot produce are still a non-tty.
        assert_eq!(classify_raw(false, true, true), RawVerdict::NoTerminal);
        // A terminal that gave up canonical mode: the happy path.
        assert_eq!(classify_raw(true, true, true), RawVerdict::Raw);
        // A terminal whose settings could not be read, or could not be written.
        assert_eq!(classify_raw(true, false, false), RawVerdict::Failed);
        assert_eq!(classify_raw(true, true, false), RawVerdict::Failed);

        assert_ne!(
            classify_raw(true, true, false),
            classify_raw(false, false, false),
            "a terminal that refused the mode change is not a pipe: one earns BR-11's notice \
             and the other must stay silent"
        );

        // The polarity, as a pair. The two classifiers name the same state from
        // the same three facts; a future edit that "fixed" the fail-open by
        // reclassifying rather than by changing the caller would break here.
        for (is_tty, got, set) in [
            (false, false, false),
            (true, true, true),
            (true, true, false),
        ] {
            assert_eq!(
                classify_raw(is_tty, got, set) == RawVerdict::Failed,
                classify_echo(is_tty, got, set) == EchoOutcome::Failed,
                "the split between fail-open and fail-closed lives at the callers, not in the \
                 classifiers: {is_tty} {got} {set}"
            );
        }
    }

    // ---- REQ-622 BR-2 / BR-5: a question's answer, with no terminal --------

    /// A stand-in for [`read_available`]: the chunks a terminal would have
    /// handed over, one per pass, then nothing.
    ///
    /// Chunks rather than one string, because *where the reads split* is half
    /// of what [`read_answer_raw`] has to get right — a terminal delivers `é`
    /// as whatever bytes were ready, and the split is exactly where a decoder
    /// that assembled per-read rather than per-character would lose the tail.
    ///
    /// Exhaustion reports `Ok(0)`, which is what a real stdin with nothing
    /// waiting reports; paired with a `wait` that says "readable" it drives the
    /// [`READY_BUT_EMPTY_IS_EOF`] path, so a script that never ends its line
    /// makes the read *return* rather than hang.
    ///
    /// A closure factory, and deliberately not shared with `client.rs`'s
    /// same-named stand-in: that one is a `fn` pointer, because the pump reaches
    /// its reader through a field of that type, and this one is a closure
    /// because the prompter's reader is a parameter. Two shapes for two seams —
    /// what must not be duplicated is the *decoding*, and that is
    /// `InputEditor`'s, reached by both.
    fn scripted_keys(script: &[&[u8]]) -> impl FnMut(&mut [u8]) -> io::Result<usize> {
        let mut passes: std::collections::VecDeque<Vec<u8>> =
            script.iter().map(|chunk| chunk.to_vec()).collect();
        move |buf| {
            let Some(chunk) = passes.pop_front() else {
                return Ok(0);
            };
            let take = chunk.len().min(buf.len());
            buf[..take].copy_from_slice(&chunk[..take]);
            Ok(take)
        }
    }

    /// This file's production half, for the region checks below.
    ///
    /// [`handler_body`]'s corpus, cut the same way and for the same reason: the
    /// needles are written in this module, so an uncut corpus would match them
    /// and pass with the production code containing nothing at all.
    fn production_source() -> &'static str {
        const SOURCE: &str = include_str!("prompt.rs");
        SOURCE
            .split_once("\n#[cfg(test)]")
            .map_or(SOURCE, |(before, _)| before)
    }

    /// REQ-622 BR-2: in raw mode the answer is **assembled by the editor** —
    /// one reader, one decoder — and echoed by the prompter into the question's
    /// own row.
    ///
    /// The script is the whole claim, because every keystroke in it is one a
    /// `read_line` could not have handled and a byte-echoing writer could not
    /// have painted:
    ///
    /// - `a`, `b`, then Backspace: the kernel is not implementing Backspace any
    ///   more, so a reader that merely collected bytes would answer `ab\x7f`;
    /// - `é` split across two passes: the two halves of one character arrive in
    ///   separate reads, and a decoder working per-read drops the tail;
    /// - `ESC [ A`: an arrow key, which must echo **nothing** and change
    ///   nothing (BR-9) — a passthrough echo would paint `[A` into the answer;
    /// - `\r\n`: one Enter, not two.
    ///
    /// The echo is asserted as one literal covering every repaint in order
    /// (LESSON-569: the oracle is written out, never recomputed with the
    /// subject's own expression). Five repaints for five changes to the line,
    /// and not one for the arrow key or for the held half of the `é`.
    #[test]
    fn a_raw_mode_answer_is_read_through_the_editor() {
        let mut answer = InputEditor::default();
        let mut echo: Vec<u8> = Vec::new();

        let line = read_answer_raw(
            &mut answer,
            "allow? ",
            &mut echo,
            scripted_keys(&[b"ab", b"\x7f", &[0xc3], &[0xa9], b"\x1b[A", b"!", b"\r\n"]),
            || true,
        );

        assert_eq!(
            line.as_deref(),
            Some("aé!"),
            "the answer is what the editor assembled: Backspace removed a whole \
             character, the split `é` survived, and the arrow key changed nothing"
        );
        assert_eq!(
            String::from_utf8(echo).expect("the echo is utf-8"),
            "\r\x1b[Kallow? a\
             \r\x1b[Kallow? ab\
             \r\x1b[Kallow? a\
             \r\x1b[Kallow? aé\
             \r\x1b[Kallow? aé!",
            "each change repaints the question's row once, from the editor's \
             text; a held half-character and a consumed escape sequence repaint \
             nothing"
        );
        assert_eq!(
            answer.queued_len(),
            0,
            "an answer is never a queued prompt (BR-5): the buffer is shelved for \
             the read, so Enter holds the line for `take_answer` instead of \
             pushing it onto the queue"
        );

        // **The wiring, pinned where it lives** ([[LESSON-547]],
        // [[LESSON-568]]). The behaviour above is `read_answer_raw`'s; that
        // both `ask`s *reach* it, and reach it off the terminal's own state
        // rather than off a flag, cannot be driven from a unit test — the other
        // branch reads the real descriptor 0. So it is asserted about the
        // source, cut at the first column-zero `#[cfg(test)]` so these needles
        // cannot match themselves.
        let source = production_source();
        let region = |start: &str, end: &str| -> &str {
            let at = source
                .find(start)
                .unwrap_or_else(|| panic!("this sweep's anchor is gone: {start:?}"));
            let len = source[at..]
                .find(end)
                .unwrap_or_else(|| panic!("{end:?} no longer follows {start:?}"));
            &source[at..at + len]
        };
        for (start, end) in [
            ("impl Prompter for StdinPrompter {", "fn ask_secret"),
            (
                "impl Prompter for FramedStdinPrompter {",
                "/// A credential is never asked for inside the entry frame.",
            ),
        ] {
            let ask = region(start, end);
            assert!(
                ask.contains("RawMode::is_engaged()"),
                "`{start}`'s `ask` must branch on the terminal's own mode — a \
                 `read_line` against a kernel that has stopped assembling lines \
                 hangs rather than fails (BR-2)"
            );
            assert!(
                ask.contains("read_answer_raw("),
                "`{start}`'s raw branch must read through the one answer reader, \
                 not a second one of its own (ADR-622-2)"
            );
        }
    }

    /// REQ-622 BR-5: a question reads **only** the keystrokes typed after it was
    /// drawn, and the line the user was part-way through is neither its answer
    /// nor consumed by it.
    ///
    /// Two buffers, which is the mechanism rather than a restatement of the
    /// rule. The prompter's own buffer is dirtied first — that stands for
    /// anything that could have got into it before the question's first row was
    /// painted, the tail of a previous question included — and the read has to
    /// answer `y` rather than `rm -rf /y`. Meanwhile the *pump's* editor,
    /// shelved where the question was drawn, comes back verbatim and with an
    /// untouched queue: the answer read never reaches it.
    ///
    /// **The benign path is the dangerous one here.** Every assertion below
    /// passes against a prompter that shares one buffer with the pump *as long
    /// as nothing was pending* — which is the common case, and is why the buffer
    /// is pre-loaded rather than left empty. What this guards against is a
    /// permission prompt answered `y` by a line the user typed half a second
    /// before the question they never saw.
    #[test]
    fn type_ahead_before_a_question_is_not_its_answer() {
        // The pump's editor: the user's half-written line, shelved at the moment
        // the question was drawn (TASK-417).
        let mut pump = InputEditor::default();
        pump.push("half a thought".as_bytes());
        pump.shelve();

        // The prompter's own buffer, dirty.
        let mut answer = InputEditor::default();
        answer.push(b"rm -rf /");
        assert_eq!(
            answer.answer_so_far(),
            "rm -rf /",
            "the fixture is worth nothing unless the buffer really is dirty"
        );

        let mut echo: Vec<u8> = Vec::new();
        let line = read_answer_raw(
            &mut answer,
            "allow? ",
            &mut echo,
            scripted_keys(&[b"y", b"\n"]),
            || true,
        );

        assert_eq!(
            line.as_deref(),
            Some("y"),
            "the answer is the keystroke typed after the question, and nothing \
             that was in the buffer before it"
        );
        assert_eq!(
            String::from_utf8(echo).expect("the echo is utf-8"),
            "\r\x1b[Kallow? y",
            "and the row the user reads shows what the caller got — a question \
             whose row said `rm -rf /y` would be answered blind"
        );

        pump.unshelve();
        assert_eq!(
            pump.answer_so_far(),
            "half a thought",
            "the shelved line comes back verbatim: one that came back subtly \
             different would be worse than one that came back empty"
        );
        assert_eq!(
            pump.queued_len(),
            0,
            "and the question consumed nothing from the queue either (BR-5)"
        );
    }

    /// REQ-622 BR-5, the other end of the same read: a paste answers **one**
    /// question and the rest of that read is dropped.
    ///
    /// A shelved editor's Enter leaves the line in the buffer for `take_answer`,
    /// so a reader that pushed a whole read in one call would hand back `yn` —
    /// the first line's answer with the second line's keystrokes appended, from
    /// a question the user answered once.
    #[test]
    fn a_paste_answers_one_question_and_the_rest_of_that_read_is_dropped() {
        let mut answer = InputEditor::default();
        let mut echo: Vec<u8> = Vec::new();
        let line = read_answer_raw(
            &mut answer,
            "allow? ",
            &mut echo,
            scripted_keys(&[b"y\nn\n"]),
            || true,
        );
        assert_eq!(line.as_deref(), Some("y"));
        assert_eq!(answer.queued_len(), 0);
    }

    /// REQ-622: a descriptor that says it is readable and hands over nothing is
    /// the end of the input, not a loop to sit in.
    ///
    /// The cancel [`Prompter::ask`] already contracts for, reached with no
    /// terminal and no clock: the scripted source runs dry, the wait claims
    /// readable every pass, and the read gives up after
    /// [`READY_BUT_EMPTY_IS_EOF`] of them. The alternative — trusting one
    /// zero-byte read — would cancel a question on the stray signal
    /// [`read_available`] maps to `Ok(0)`.
    #[test]
    fn an_answer_read_gives_up_on_a_descriptor_that_hands_over_nothing() {
        let mut answer = InputEditor::default();
        let mut echo: Vec<u8> = Vec::new();
        let mut passes = 0usize;
        let line = read_answer_raw(
            &mut answer,
            "allow? ",
            &mut echo,
            scripted_keys(&[b"y"]),
            || {
                passes += 1;
                true
            },
        );
        assert_eq!(line, None, "an exhausted descriptor is a cancel");
        assert_eq!(
            passes, READY_BUT_EMPTY_IS_EOF,
            "and it is reached in a bounded number of passes, so the question \
             returns rather than hangs"
        );
        assert_eq!(
            String::from_utf8(echo).expect("the echo is utf-8"),
            "\r\x1b[Kallow? y",
            "what was typed before the descriptor ended was still painted once"
        );
    }

    /// REQ-560 BR-11, extended to the one place a queued line is ever shown
    /// (REQ-622 BR-4, ADR-622-5).
    ///
    /// The frame is the same frame: the queued line is written into the input
    /// row *after* the cursor has risen into it, so the two rules and the status
    /// row below them are placed by exactly the counts an empty frame uses. What
    /// differs is the advance — nothing was typed, so the terminal echoed no
    /// newline, and the cursor needs the same two rows EOF needs. One newline
    /// here would leave the next line of output sitting on the bottom rule.
    #[test]
    fn a_queued_lines_frame_echoes_it_once_and_advances_as_enter_would() {
        let mut p = FramedStdinPrompter::new(true, false);
        let rule = p.rule();
        assert_eq!(
            p.submitted_bytes("> ", "cargo test"),
            format!("{rule}\n\n{rule}\n\x1b[2A> cargo test\n\n"),
            "the line is shown once, in the input row, and the cursor is stepped \
             past the frame as a typed Enter would have left it"
        );
        assert_eq!(p.below_rows, 0);

        // With a status row every count moves together, exactly as for a typed
        // line.
        p.set_status(Some("permissions: guarded".to_owned()));
        let bytes = p.submitted_bytes("> ", "cargo test");
        assert!(bytes.ends_with("\x1b[3A> cargo test\n\n\n"), "{bytes:?}");
        assert_eq!(p.below_rows, 1);

        // A queued line is the user's own text, and it is still text a terminal
        // reads as commands: the row it is echoed into must not be able to move
        // the cursor out of the frame it is drawn in.
        let hostile = p.submitted_bytes("> ", "one\x1b[2Atwo\r");
        assert_eq!(
            hostile.matches("\x1b[").count(),
            1,
            "the frame's own cursor escape is the only escape in it: {hostile:?}"
        );
        assert!(!hostile.contains('\r'), "{hostile:?}");

        // With the frame off nothing is written at all — the piped path stays
        // byte-identical (BR-1, AC-7).
        let mut piped = FramedStdinPrompter::new(false, false);
        piped.draw_submitted("> ", "cargo test");
        assert_eq!(
            piped.below_rows, 0,
            "an unframed prompter must not accrue rows to step over"
        );
    }
}
