//! The rendering seam.
//!
//! Every character the CLI shows goes through a [`Surface`]. The MVP ships one
//! implementation, [`PlainSurface`], that writes plain streaming text — but the
//! whole UI is written against the trait, not against `stdout`, so a future
//! ratatui front-end is a new `Surface` impl and nothing else changes (the
//! technical-note requirement: "isolate rendering behind a small trait").
//!
//! The trait is deliberately tiny: a semantic [`LineKind`] tag plus two verbs —
//! [`Surface::line`] for a complete, newline-terminated line, and
//! [`Surface::fragment`] for a chunk of streamed text with no trailing newline
//! (assistant output arrives token-by-token). Tests drive scripted event streams
//! through a [`RecordingSurface`] and assert on the semantic `(kind, text)` pairs
//! rather than on any particular byte formatting.

use std::io::{self, Write};

/// The semantic class of a rendered line. A concrete [`Surface`] decides how each
/// class looks (a prefix now, a coloured pane later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A one-line control notice — routing, privacy, degradation, phase,
    /// model-lifecycle. These are the legibility promise (BR-5): every control
    /// decision is visible.
    Notice,
    /// A tool-call status line.
    Tool,
    /// A line of a proposed diff.
    Diff,
    /// An interactive prompt (e.g. a permission question header).
    Prompt,
    /// Cost-meter output.
    Cost,
    /// Neutral informational text (session ready, plan entries, attaches).
    Info,
    /// An error line.
    Error,
}

/// The rendering target. See the module docs for the contract.
pub trait Surface {
    /// Emit one complete, newline-terminated line of the given semantic class.
    fn line(&mut self, kind: LineKind, text: &str);

    /// Emit a fragment of streamed text with no trailing newline. Used for
    /// assistant output, which arrives as a sequence of chunks.
    fn fragment(&mut self, text: &str);

    /// Flush any buffered output. The default is a no-op.
    ///
    /// # Errors
    ///
    /// Returns any error the underlying writer raises while flushing.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A plain streaming-text surface over any [`Write`] (stdout in the binary).
///
/// It tracks whether the cursor is at the start of a line so that a `line()`
/// arriving in the middle of streamed `fragment()`s first closes the open line —
/// keeping notices and assistant text from colliding on one row.
pub struct PlainSurface<W: Write> {
    out: W,
    at_line_start: bool,
}

impl<W: Write> PlainSurface<W> {
    /// Wraps `out` in a surface. Starts assuming a fresh line.
    pub fn new(out: W) -> Self {
        Self {
            out,
            at_line_start: true,
        }
    }

    /// The prefix shown for a line class. Cosmetic only — tests assert on the
    /// semantic class, never on this string.
    fn prefix(kind: LineKind) -> &'static str {
        match kind {
            LineKind::Notice => ">> ",
            LineKind::Tool => " - ",
            LineKind::Diff => "",
            LineKind::Prompt => "? ",
            LineKind::Cost => "",
            LineKind::Info => "",
            LineKind::Error => "error: ",
        }
    }
}

/// A convenience constructor for the common case: a surface over stdout.
#[must_use]
pub fn stdout_surface() -> PlainSurface<io::Stdout> {
    PlainSurface::new(io::stdout())
}

impl<W: Write> Surface for PlainSurface<W> {
    fn line(&mut self, kind: LineKind, text: &str) {
        // Close any open streamed line first so the notice starts clean.
        if !self.at_line_start {
            let _ = writeln!(self.out);
        }
        let _ = writeln!(self.out, "{}{}", Self::prefix(kind), text);
        self.at_line_start = true;
    }

    fn fragment(&mut self, text: &str) {
        let _ = write!(self.out, "{text}");
        self.at_line_start = text.ends_with('\n');
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// One thing a surface was asked to render, captured for assertions.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rendered {
    /// A `line(kind, text)` call.
    Line(LineKind, String),
    /// A `fragment(text)` call.
    Fragment(String),
}

/// A [`Surface`] that records every call instead of writing bytes. Test-only.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RecordingSurface {
    /// Every render call, in order.
    pub calls: Vec<Rendered>,
}

#[cfg(test)]
impl RecordingSurface {
    /// A fresh recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The concatenation of every fragment written (the streamed assistant text).
    pub fn fragments(&self) -> String {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Rendered::Fragment(t) => Some(t.as_str()),
                Rendered::Line(..) => None,
            })
            .collect()
    }

    /// All line texts of a given kind, in order.
    pub fn lines_of(&self, kind: LineKind) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Rendered::Line(k, t) if *k == kind => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// True if any recorded line of `kind` contains `needle`.
    pub fn any_line_contains(&self, kind: LineKind, needle: &str) -> bool {
        self.lines_of(kind).iter().any(|t| t.contains(needle))
    }
}

#[cfg(test)]
impl Surface for RecordingSurface {
    fn line(&mut self, kind: LineKind, text: &str) {
        self.calls.push(Rendered::Line(kind, text.to_owned()));
    }

    fn fragment(&mut self, text: &str) {
        self.calls.push(Rendered::Fragment(text.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_surface_closes_an_open_fragment_before_a_line() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.fragment("hello");
            surface.line(LineKind::Notice, "routed to local");
        }
        let text = String::from_utf8(buf).unwrap();
        // The fragment is closed with a newline before the notice appears.
        assert!(text.starts_with("hello\n"));
        assert!(text.contains("routed to local"));
    }

    #[test]
    fn plain_surface_does_not_inject_a_newline_when_already_at_line_start() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.line(LineKind::Info, "one");
            surface.line(LineKind::Info, "two");
        }
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text, "one\ntwo\n");
    }

    #[test]
    fn recording_surface_captures_kinds_and_fragments() {
        let mut surface = RecordingSurface::new();
        surface.fragment("chunk-a");
        surface.fragment("chunk-b");
        surface.line(LineKind::Notice, "note one");
        surface.line(LineKind::Error, "boom");

        assert_eq!(surface.fragments(), "chunk-achunk-b");
        assert_eq!(surface.lines_of(LineKind::Notice), vec!["note one"]);
        assert!(surface.any_line_contains(LineKind::Error, "boom"));
        assert!(!surface.any_line_contains(LineKind::Notice, "boom"));
    }
}
