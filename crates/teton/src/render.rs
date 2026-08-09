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

    /// Repaint one row `rows_up` above the cursor **in place**, leaving the
    /// cursor exactly where it was (REQ-556 ADR-556-4).
    ///
    /// The loading indicator's animation uses this rather than redrawing the
    /// entry frame. In canonical mode the terminal echoes keystrokes into the
    /// input row while the kernel holds the line until Enter; a frame redraw
    /// every animation interval would blank those echoed characters several
    /// times a second. The text would still be delivered — but watching it
    /// flicker away while typing is not a thing to ship.
    ///
    /// `rows_up` comes from the caller because frame geometry is the caller's
    /// knowledge, not the surface's; the surface owns only how to move a cursor.
    ///
    /// **Defaults to a no-op**, which is BR-2's guarantee for every surface
    /// that is not a terminal — including any future one. A surface with no
    /// cursor has no row to repaint, so silence is the correct behaviour rather
    /// than something each implementor must remember to add.
    fn repaint_row_above(&mut self, _rows_up: usize, _kind: LineKind, _text: &str) {}

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

/// Replace every control character in `text` with a space, keeping tabs.
///
/// A terminal reads control characters as *commands*, so text that reaches one
/// unfiltered is text that can move the cursor, erase rows, and rewrite what the
/// user already read. That is not a hypothetical for this surface: a permission
/// description carries a model-composed URL (REQ-563), and
/// `…https://good.example\x1b[2K\x1b[1A…https://evil.example` redraws the very
/// line that asked the user to approve a host — the consent prompt then displays
/// one destination and authorizes another. Neutralizing the escapes leaves the
/// characters visible as text, which is the honest rendering: the page really
/// did contain them.
///
/// Tabs are kept because they are the one control character that is ordinary
/// *content* here — a diff line of indented source is a normal thing to render —
/// and because a tab advances the cursor within a row exactly as a space does.
/// It cannot move up, erase, or start a new line, which is the whole capability
/// this is removing.
///
/// This is LESSON-474's rule again — sanitize where the parser is. The parser is
/// the terminal, so the guard belongs at the writer that feeds it rather than at
/// each of the ~180 call sites that compose a line, any one of which could
/// forget.
fn defused(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\t' || !c.is_control() { c } else { ' ' })
        .collect()
}

impl<W: Write> Surface for PlainSurface<W> {
    fn line(&mut self, kind: LineKind, text: &str) {
        // Close any open streamed line first so the notice starts clean.
        if !self.at_line_start {
            let _ = writeln!(self.out);
        }
        let _ = writeln!(self.out, "{}{}", Self::prefix(kind), defused(text));
        self.at_line_start = true;
    }

    fn fragment(&mut self, text: &str) {
        let _ = write!(self.out, "{text}");
        self.at_line_start = text.ends_with('\n');
    }

    /// Save the cursor, step up, clear that row, write, restore. `at_line_start`
    /// is deliberately untouched: the cursor ends where it began, so the
    /// bookkeeping that keeps a later `line()` from colliding with streamed
    /// output is still accurate.
    fn repaint_row_above(&mut self, rows_up: usize, kind: LineKind, text: &str) {
        let prefix = Self::prefix(kind);
        // A repaint claims exactly one row, and the cursor restore assumes it: a
        // newline here would scroll the frame out from under `\x1b[u` and leave
        // the entry area shredded. That is the sharper consequence, but it is
        // not a different rule — [`defused`] is what `line()` uses too, for the
        // reason written there.
        let single_row = defused(text);
        let _ = write!(
            self.out,
            "\x1b[s\x1b[{rows_up}A\r\x1b[K{prefix}{single_row}\x1b[u"
        );
        let _ = self.out.flush();
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
    /// A `repaint_row_above(rows_up, kind, text)` call — recorded distinctly
    /// from `Line` so a test can tell "scrolled a new line into the log" from
    /// "redrew a row in place", which is exactly the distinction ADR-556-4 is
    /// about.
    Repaint(usize, LineKind, String),
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
                Rendered::Line(..) | Rendered::Repaint(..) => None,
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

    fn repaint_row_above(&mut self, rows_up: usize, kind: LineKind, text: &str) {
        self.calls
            .push(Rendered::Repaint(rows_up, kind, text.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-556 ADR-556-4 / AC-5. The animation must repaint its row **in
    /// place** — save, move, clear, write, restore — and must not disturb the
    /// cursor. The first implementation tore the entry frame down and redrew it
    /// on every tick, which blanked whatever the user had typed into the input
    /// row eight times a second; the text still arrived on Enter, but it
    /// visibly flickered away as it was typed.
    #[test]
    fn a_repaint_restores_the_cursor_and_leaves_line_bookkeeping_alone() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            // Mid-stream, so the `at_line_start` bookkeeping is in its
            // interesting state.
            surface.fragment("partially typed");
            surface.repaint_row_above(2, LineKind::Notice, "model starting..");
            // A repaint must not have changed where the surface thinks it is —
            // the cursor came back to exactly where it was, so a later `line()`
            // still knows it must close the open fragment first.
            surface.line(LineKind::Info, "after");
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[s"), "saves the cursor: {out:?}");
        assert!(
            out.contains("\x1b[2A"),
            "steps up to the status row: {out:?}"
        );
        assert!(out.contains("\x1b[K"), "clears only that row: {out:?}");
        assert!(out.contains("\x1b[u"), "restores the cursor: {out:?}");
        assert!(
            !out.contains("\x1b[J"),
            "must not clear to end of screen — that is the frame teardown this \
             replaced, and it is what erased typed input: {out:?}"
        );
        // The repaint carries the same prefix the scrolled line would, so the
        // row does not visibly jump when the indicator is finally replaced by
        // `render_lifecycle`'s own notice.
        assert!(out.contains(">> model starting.."), "{out:?}");
        // `at_line_start` was left alone: the fragment is still open, so the
        // following `line()` closed it with a newline first.
        assert!(
            out.contains("partially typed"),
            "the streamed fragment survived: {out:?}"
        );
    }

    /// A repaint owns exactly one row and its cursor restore depends on that,
    /// so a control character in the text — which would scroll the frame out
    /// from under the restore — is defused at the writer rather than trusted
    /// from the source (LESSON-474).
    #[test]
    fn a_repaint_cannot_be_made_to_span_more_than_its_row() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.repaint_row_above(2, LineKind::Notice, "model\nstarting\r\x1b[2J..");
        }
        let out = String::from_utf8(buf).unwrap();
        let body = out
            .split("\x1b[K")
            .nth(1)
            .expect("the cleared-row body")
            .split("\x1b[u")
            .next()
            .expect("up to the cursor restore");
        assert!(
            !body.contains('\n') && !body.contains('\r') && !body.contains('\x1b'),
            "control characters must not survive into the repainted row: {body:?}"
        );
    }

    /// BR-2's guarantee for every non-terminal surface, including future ones:
    /// the default is silence, so a new `Surface` implementor cannot forget to
    /// suppress the indicator.
    #[test]
    fn a_surface_that_does_not_override_repaint_emits_nothing() {
        struct Bare(Vec<String>);
        impl Surface for Bare {
            fn line(&mut self, _kind: LineKind, text: &str) {
                self.0.push(text.to_owned());
            }
            fn fragment(&mut self, _text: &str) {}
        }
        let mut bare = Bare(Vec::new());
        bare.repaint_row_above(2, LineKind::Notice, "model starting..");
        assert!(
            bare.0.is_empty(),
            "the default repaint must be a no-op: {:?}",
            bare.0
        );
    }

    /// A consent prompt that can be redrawn by the thing it is asking about is
    /// not a consent prompt. A permission description carries a model-composed
    /// URL (REQ-563), and an escape sequence in it used to reach the terminal
    /// intact — enough to erase the row naming the host and print a different
    /// one over it, so the user approves what they were never shown.
    #[test]
    fn a_line_cannot_redraw_the_prompt_it_is_part_of() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.line(
                LineKind::Prompt,
                "permission requested: web_fetch_any_url — fetch https://good.example\
                 \x1b[2K\x1b[1Afetch https://evil.example",
            );
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains('\x1b'),
            "an escape sequence reached the terminal: {out:?}"
        );
        // Exactly one row was claimed — the trailing newline `line()` writes and
        // nothing else.
        assert_eq!(out.matches('\n').count(), 1, "{out:?}");
        // Neutralized, not censored: the text really was on the page, and the
        // user is better served seeing it than seeing a gap.
        assert!(out.contains("good.example"), "{out:?}");
        assert!(out.contains("evil.example"), "{out:?}");
    }

    /// The C0 set is not just `ESC`: a carriage return alone reprints over the
    /// line's own start, and a backspace walks back over what was written.
    #[test]
    fn every_control_character_a_line_carries_is_neutralized_except_tab() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.line(LineKind::Diff, "+ \tif x {\r\x08\x1b[1A\u{9b}2Kelse {");
        }
        let out = String::from_utf8(buf).unwrap();
        for banned in ['\r', '\x08', '\x1b', '\u{9b}'] {
            assert!(
                !out.contains(banned),
                "{banned:?} survived into a rendered line: {out:?}"
            );
        }
        // A tab is content, not a command: a diff of indented source must still
        // look indented.
        assert!(out.contains("+ \tif x {"), "{out:?}");
    }

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
