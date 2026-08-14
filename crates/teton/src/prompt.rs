//! The interactive-input seam.
//!
//! Anything that reads a line from the user goes through a [`Prompter`], so the
//! permission round-trip (event in → question → answer → `permission/respond`
//! out) can be unit-tested with scripted answers and no terminal. The binary
//! wires in [`StdinPrompter`]; tests wire in a scripted one.

use std::io::{self, Write};

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
pub struct StdinPrompter;

impl StdinPrompter {
    /// A new stdin-backed prompter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Prompter for StdinPrompter {
    fn ask(&mut self, question: &str) -> Option<String> {
        let mut out = io::stdout();
        let _ = write!(out, "{}", defused(question));
        let _ = out.flush();
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
            // it looks exactly like a working prompt and leaks the key.
            EchoState::Failed => {
                let _ = writeln!(out);
                let _ = writeln!(out, "{ECHO_UNAVAILABLE}");
                let _ = out.flush();
                return None;
            }
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

/// Terminal echo, switched off for the life of the guard and restored on drop
/// (REQ-572 AC-5).
///
/// Canonical mode is deliberately left alone: only `ECHO` is cleared, so the
/// kernel still assembles the line and `read_line` behaves exactly as it does
/// for every other prompt — the one difference is that the characters are not
/// painted back.
///
/// **Accepted residual**: a signal that kills the process between `engage` and
/// the drop (Ctrl-C at the key prompt) leaves the user's terminal with echo off
/// until they run `stty sane`, because `Drop` does not run for a process the
/// kernel terminates. Every `read -s`-style prompt without a signal handler has
/// this window, and installing one from a CLI that has no other signal handling
/// would be a larger change than the wart it removes. The ordinary abort paths
/// — EOF, an empty answer — return through the guard normally and restore.
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
        let set_attrs = got_attrs && {
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
            EchoOutcome::Hidden => EchoState::Hidden(Self { saved }),
            EchoOutcome::NoTerminal => EchoState::NoTerminal,
            EchoOutcome::Failed => EchoState::Failed,
        }
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
        let bytes = self.draw_bytes(question);
        let mut out = io::stdout();
        let _ = write!(out, "{bytes}");
        let _ = out.flush();
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
    fn draw_bytes(&mut self, question: &str) -> String {
        let question = defused(question);
        // Styling happens HERE, after defusing, or not at all. A caller
        // hand-composing SGR into the question hands the sanitizer exactly the
        // bytes it exists to destroy — the REQ-573 verify pass briefly shipped
        // that as literal `[36m` debris where the chevron's tint had been. The
        // seam owns the tint; callers hand in plain text.
        let question = if self.color {
            question.replace('›', "\x1b[36m›\x1b[0m")
        } else {
            question
        };
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
        bytes.push_str(&format!("\x1b[{up}A{question}"));
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
    /// Both then have to clear [`Self::below_rows`] more, and that is the whole
    /// of REQ-560's stranding hazard: with a status row below the bottom rule,
    /// the pre-REQ single newline would have parked the cursor **on** the status
    /// row and let the next output overwrite it in place, leaving whatever was
    /// wider than that output stranded behind it. The count comes from the same
    /// field [`Self::draw_bytes`] set, so the rows stepped over are exactly the
    /// rows written.
    fn advance_bytes(&self, eof: bool) -> String {
        let rows = if eof { 2 } else { 1 } + self.below_rows;
        "\n".repeat(rows)
    }
}

impl Prompter for FramedStdinPrompter {
    fn ask(&mut self, question: &str) -> Option<String> {
        if !self.framed {
            return StdinPrompter::new().ask(question);
        }
        self.draw(question);
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

    /// Without a status row the frame is byte-identical to the pre-REQ-560 one.
    ///
    /// This is the assertion that keeps the change to the *un*-configured case
    /// zero — including the non-interactive path, where `framed` is false and
    /// nothing is written at all (BR-9).
    #[test]
    fn a_frame_with_no_status_row_is_the_frame_it_always_was() {
        let mut p = FramedStdinPrompter::new(true, false);
        let bytes = p.draw_bytes("> ");
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
        let bytes = p.draw_bytes("> ");

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
        let bytes = p.draw_bytes("> ");
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
        let _ = p.draw_bytes("> ");
        assert_eq!(p.below_rows, 1);

        p.set_status(None);
        let bytes = p.draw_bytes("> ");
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
        let bytes = p.draw_bytes(hostile);
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
            let bytes = p.draw_bytes(ordinary);
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
        let bytes = tinted.draw_bytes(" › ");
        assert!(
            bytes.contains(" \x1b[36m›\x1b[0m "),
            "the seam must tint the chevron the caller handed in plain: {bytes:?}"
        );

        // Colour off: the same question stays plain, and no SGR appears.
        let mut plain = FramedStdinPrompter::new(true, false);
        let bytes = plain.draw_bytes(" › ");
        assert!(
            bytes.contains(" › ") && !bytes.contains("\x1b[36m"),
            "no colour means no SGR at all: {bytes:?}"
        );

        // A hostile question is defused whether or not the tint applies — the
        // erase and the carriage return die, the chevron still gets its tint.
        let mut hostile = FramedStdinPrompter::new(true, true);
        let bytes = hostile.draw_bytes("\x1b[2K› \r");
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
}
