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
