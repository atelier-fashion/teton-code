//! The interactive-input seam.
//!
//! Anything that reads a line from the user goes through a [`Prompter`], so the
//! permission round-trip (event in → question → answer → `permission/respond`
//! out) can be unit-tested with scripted answers and no terminal. The binary
//! wires in [`StdinPrompter`]; tests wire in a scripted one.

use std::io::{self, Write};

/// A source of interactive answers.
pub trait Prompter {
    /// Show `question` and read one line of input. Returns `None` on EOF (the
    /// user pressed Ctrl-D), which callers treat as a cancel.
    fn ask(&mut self, question: &str) -> Option<String>;
}

/// The real prompter: writes the question to stdout and reads a line from stdin.
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
        let _ = write!(out, "{question}");
        let _ = out.flush();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => None, // EOF
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
            Err(_) => None,
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
    fn draw_bytes(&mut self, question: &str) -> String {
        let rule = self.rule();
        // Rule, blank input row, rule, then the status row if there is one.
        let mut bytes = format!("{rule}\n\n{rule}\n");
        self.below_rows = match &self.status {
            Some(status) => {
                bytes.push_str(status);
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
            let _ = write!(out, "{}", self.advance_bytes(matches!(read, Ok(0) | Err(_))));
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
}

#[cfg(test)]
impl ScriptedPrompter {
    /// Builds a prompter that will hand back `answers` in order.
    pub fn new(answers: &[&str]) -> Self {
        Self {
            answers: answers.iter().map(|s| (*s).to_owned()).collect(),
            asked: 0,
            questions: Vec::new(),
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

    #[test]
    fn scripted_prompter_replays_then_reports_eof() {
        let mut p = ScriptedPrompter::new(&["y", "n"]);
        assert_eq!(p.ask("q1"), Some("y".to_owned()));
        assert_eq!(p.ask("q2"), Some("n".to_owned()));
        assert_eq!(p.ask("q3"), None);
        assert_eq!(p.asked, 3);
    }
}
