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
}

impl FramedStdinPrompter {
    /// A new entry prompter. `framed` gates the frame, `color` the dimming.
    #[must_use]
    pub fn new(framed: bool, color: bool) -> Self {
        Self { framed, color }
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
        let mut out = io::stdout();
        let rule = self.rule();
        // Rule, blank input row, rule — then cursor up two, into the input row.
        let _ = write!(out, "{rule}\n\n{rule}\n\x1b[2A{question}");
        let _ = out.flush();
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
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                if self.framed {
                    // EOF echoes no newline: the cursor is still on the input
                    // row, two steps above where output should resume.
                    let _ = writeln!(out, "\n");
                    let _ = out.flush();
                }
                None
            }
            Ok(_) => {
                if self.framed {
                    // Enter's echo landed the cursor on the bottom rule; step past.
                    let _ = writeln!(out);
                    let _ = out.flush();
                }
                Some(line.trim_end_matches(['\n', '\r']).to_owned())
            }
        }
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

/// The terminal's column count, or a conservative 80 when stdout is not a
/// terminal or the query fails.
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

fn terminal_width() -> usize {
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
}

#[cfg(test)]
impl ScriptedPrompter {
    /// Builds a prompter that will hand back `answers` in order.
    pub fn new(answers: &[&str]) -> Self {
        Self {
            answers: answers.iter().map(|s| (*s).to_owned()).collect(),
            asked: 0,
        }
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn ask(&mut self, _question: &str) -> Option<String> {
        self.asked += 1;
        self.answers.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_prompter_replays_then_reports_eof() {
        let mut p = ScriptedPrompter::new(&["y", "n"]);
        assert_eq!(p.ask("q1"), Some("y".to_owned()));
        assert_eq!(p.ask("q2"), Some("n".to_owned()));
        assert_eq!(p.ask("q3"), None);
        assert_eq!(p.asked, 3);
    }
}
