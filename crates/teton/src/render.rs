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
    /// A row of the startup banner's skyline art.
    BannerArt,
    /// The banner's identity line — product, version, tagline.
    BannerTitle,
    /// A secondary banner line, subordinate to the title (the working directory).
    BannerMeta,
}

impl LineKind {
    /// The SGR parameters this class is drawn with, or `None` for classes that
    /// carry no styling of their own.
    ///
    /// Styling lives here rather than in the text a caller hands to
    /// [`Surface::line`] because [`neutralized`] deliberately strips every escape
    /// out of that text — a caller that embeds `\x1b[36m` in its own string gets
    /// the ESC replaced by a space and the bare `[36m` printed as literal
    /// characters. That is the guard working as designed; the escape it refuses is
    /// indistinguishable from the ones a fetched page tries to smuggle through.
    /// So the surface authors the escape itself, from a fixed table, after the
    /// text has been defused.
    fn sgr(self) -> Option<&'static str> {
        match self {
            LineKind::BannerArt => Some("36"),
            LineKind::BannerTitle => Some("1"),
            LineKind::BannerMeta => Some("2"),
            _ => None,
        }
    }
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
    color: bool,
}

impl<W: Write> PlainSurface<W> {
    /// Wraps `out` in a surface that emits no colour. Starts assuming a fresh
    /// line.
    pub fn new(out: W) -> Self {
        Self::with_color(out, false)
    }

    /// Wraps `out` in a surface that draws styled line classes with SGR when
    /// `color`. Whether the target can take colour is a property of the target,
    /// so it is the surface that holds the answer — the callers composing lines
    /// never need to know.
    pub fn with_color(out: W, color: bool) -> Self {
        Self {
            out,
            at_line_start: true,
            color,
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
            LineKind::BannerArt | LineKind::BannerTitle | LineKind::BannerMeta => "",
        }
    }
}

/// A convenience constructor for the common case: a plain surface over stdout.
#[must_use]
pub fn stdout_surface() -> PlainSurface<io::Stdout> {
    PlainSurface::new(io::stdout())
}

/// A surface over stdout that draws styled line classes in colour when `color`.
#[must_use]
pub fn stdout_surface_with_color(color: bool) -> PlainSurface<io::Stdout> {
    PlainSurface::with_color(io::stdout(), color)
}

/// Whether `c` is not a C0/C1 control but steers a terminal's *display* the same
/// way, and is therefore neutralized alongside them.
///
/// Three families, one hazard. The bidi controls (`U+202A`–`U+202E`, the
/// isolates `U+2066`–`U+2069`, and the marks `U+200E`/`U+200F`/`U+061C`) reorder
/// a row's glyphs without changing its bytes, so `https://good.example` can be
/// made to *read* as a different host than the one the consent prompt is about —
/// the Trojan-Source trick, aimed at a person rather than at a compiler. The line
/// and paragraph separators (`U+2028`/`U+2029`) are line breaks to enough
/// terminals to hand a one-row verb a second row it does not own. The zero-width
/// and joiner set (`U+200B`–`U+200D`, `U+2060`–`U+2064`, `U+00AD`, `U+FEFF`)
/// hides the seam where a spoofed host is spliced together.
///
/// Written as an explicit list rather than "every `Cf`" because `char` carries
/// no category table in `std`, and pulling a Unicode-tables crate into the CLI
/// to reach the remaining format characters — none of which reorder or break a
/// row — would cost more than the gap is worth. The list is the cheap part of
/// the category, which is the part that matters here.
fn is_display_steering(c: char) -> bool {
    matches!(c,
        '\u{00ad}'                  // SOFT HYPHEN
        | '\u{061c}'                // ARABIC LETTER MARK
        | '\u{200b}'..='\u{200f}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{2028}' | '\u{2029}'   // LINE SEPARATOR, PARAGRAPH SEPARATOR
        | '\u{202a}'..='\u{202e}'   // LRE, RLE, PDF, LRO, RLO
        | '\u{2060}'..='\u{2064}'   // WORD JOINER and the invisible operators
        | '\u{2066}'..='\u{2069}'   // LRI, RLI, FSI, PDI
        | '\u{feff}'                // ZERO WIDTH NO-BREAK SPACE (BOM)
    )
}

/// Replace every character that *commands* a terminal with a space, keeping tabs
/// — and, when `keep_newlines`, keeping `\n`.
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
/// `keep_newlines` is the one axis on which the two verbs differ.
/// [`Surface::line`] and [`Surface::repaint_row_above`] each own exactly one
/// row, so a `\n` in their text is a row they did not claim; a
/// [`Surface::fragment`] is streamed prose whose newlines are ordinary content,
/// and stripping them would reflow every multi-paragraph answer into one line. A
/// newline can only *start* a row — it cannot move up, erase, or overwrite one
/// already written — so keeping it costs the fragment path none of the guarantee.
///
/// This is LESSON-474's rule again — sanitize where the parser is. The parser is
/// the terminal, so the guard belongs at the writer that feeds it rather than at
/// each of the ~180 call sites that compose a line, any one of which could
/// forget.
fn neutralized(text: &str, keep_newlines: bool) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' || (keep_newlines && c == '\n') {
                c
            } else if c.is_control() || is_display_steering(c) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// [`neutralized`] for a verb that owns exactly one row: no newline survives.
fn defused(text: &str) -> String {
    neutralized(text, false)
}

/// [`neutralized`] for streamed prose: newlines survive, everything else that
/// commands the terminal does not.
fn defused_multiline(text: &str) -> String {
    neutralized(text, true)
}

impl<W: Write> Surface for PlainSurface<W> {
    /// The styling wraps the *defused* text and is drawn from
    /// [`LineKind::sgr`], never from the argument — so a caller cannot colour a
    /// line by embedding escapes in its string, and a fetched page cannot either.
    /// The reset is unconditional, so no styled line can leak its attribute onto
    /// the row below.
    fn line(&mut self, kind: LineKind, text: &str) {
        // A styled class is composed by this binary from fixed strings, so an
        // ESC in its text is not an attack — it is a caller reaching for SGR by
        // hand, which is the bug this styling table replaced: `defused` would
        // eat the ESC and print the bare `[36m` to the user. Silent cosmetic
        // debris is exactly the kind of thing that ships, so fail loudly in
        // development instead.
        //
        // Deliberately *not* a check on every class. `Prompt` and `Diff` carry
        // model-composed and file-derived text, where an escape is the hostile
        // input the guard exists to neutralize (REQ-563) — asserting there would
        // hand a fetched page a debug-build panic through the guard itself. The
        // constraint this places on a future styled class is the flip side: do
        // not tag untrusted text with one.
        debug_assert!(
            kind.sgr().is_none() || !text.contains('\x1b'),
            "{kind:?} is styled by the surface; it must not carry its own escapes \
             (they will be neutralized into visible debris): {text:?}"
        );

        // Close any open streamed line first so the notice starts clean.
        if !self.at_line_start {
            let _ = writeln!(self.out);
        }
        let body = defused(text);
        let _ = match kind.sgr().filter(|_| self.color) {
            Some(sgr) => writeln!(self.out, "\x1b[{sgr}m{}{body}\x1b[0m", Self::prefix(kind)),
            None => writeln!(self.out, "{}{body}", Self::prefix(kind)),
        };
        self.at_line_start = true;
    }

    /// Streamed assistant text, defused on the way out.
    ///
    /// The escapes a fetched page can steer this text into are the same escapes
    /// [`Surface::line`] refuses, aimed at the same target: a model that has just
    /// read an attacker's page can be made to emit `\x1b[2K\x1b[1A` mid-sentence
    /// and repaint the consent prompt sitting above it — a prompt whose whole job
    /// is to name the destination the *next* fetch would reach. Leaving this verb
    /// undefused would have made `line()`'s guard a guard on one of the two ways
    /// text reaches this terminal.
    ///
    /// Newlines survive here and nowhere else — see [`neutralized`] for why that
    /// costs the guarantee nothing.
    ///
    /// `at_line_start` reads the **defused** text, not the argument: a fragment
    /// ending in a bare `\r` would otherwise leave the bookkeeping claiming a
    /// fresh row while the cursor sat mid-row, and the next `line()` would print
    /// over the streamed text instead of below it.
    fn fragment(&mut self, text: &str) {
        let shown = defused_multiline(text);
        let _ = write!(self.out, "{shown}");
        self.at_line_start = shown.ends_with('\n');
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

    fn rendered(color: bool, kind: LineKind, text: &str) -> String {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_color(&mut buf, color);
            surface.line(kind, text);
        }
        String::from_utf8(buf).unwrap()
    }

    /// A styled class reaches the terminal as a real SGR sequence, opened and
    /// closed. The banner used to spell these escapes into its own line text,
    /// where [`defused`] correctly ate them and printed the remains — so the
    /// styling has to be authored *here*, past the guard, or not at all.
    #[test]
    fn a_styled_line_is_wrapped_in_sgr_and_always_reset() {
        for (kind, sgr) in [
            (LineKind::BannerArt, "\x1b[36m"),
            (LineKind::BannerTitle, "\x1b[1m"),
            (LineKind::BannerMeta, "\x1b[2m"),
        ] {
            let out = rendered(true, kind, "ridge");
            assert!(out.starts_with(sgr), "not opened with {sgr:?}: {out:?}");
            assert!(out.contains("ridge"), "text lost: {out:?}");
            assert_eq!(out.trim_end_matches('\n').rfind("\x1b["), out.rfind("\x1b[0m"));
        }
    }

    /// The colour gate is the surface's, and when it is shut the styled classes
    /// are byte-identical to plain text — no escape, and no literal `[36m`
    /// debris standing in for one.
    #[test]
    fn an_uncolored_surface_emits_no_escapes_for_styled_classes() {
        for kind in [
            LineKind::BannerArt,
            LineKind::BannerTitle,
            LineKind::BannerMeta,
        ] {
            let out = rendered(false, kind, "ridge");
            assert_eq!(out, "ridge\n", "styling leaked with colour off: {out:?}");
        }
    }

    /// Styling a class does not open a hole in the guard: the escapes come from
    /// a fixed table keyed on the class, and the *text* is defused exactly as it
    /// is for every other class. A caller cannot smuggle a cursor move through a
    /// banner line, and the row still cannot be repainted from underneath.
    ///
    /// Driven with the single-byte C1 CSI (`\u{9b}`) and a bare `\r` rather than
    /// `\x1b`, because the debug assertion in `line()` rejects an ESC on a styled
    /// class outright. These are the same capability by a different byte — which
    /// is the point: the assertion is a development guard against one authoring
    /// mistake, and `defused` is the guarantee that holds in release regardless.
    #[test]
    fn a_styled_line_still_defuses_its_text() {
        let out = rendered(true, LineKind::BannerArt, "ridge\u{9b}2K\u{9b}1A\rspoofed");
        assert!(out.starts_with("\x1b[36m"), "lost its styling: {out:?}");
        let body = out
            .trim_start_matches("\x1b[36m")
            .trim_end()
            .trim_end_matches("\x1b[0m");
        assert!(
            !body.contains('\u{9b}') && !body.contains('\r') && !body.contains('\x1b'),
            "a control character reached the terminal through a styled line: {body:?}"
        );
        assert!(body.contains("ridge"), "text lost: {body:?}");
    }

    /// The authoring mistake this whole table replaced: styling a line by
    /// spelling the SGR into its text. `defused` would eat the ESC and print the
    /// bare `[36m` to the user — cosmetic, silent, and it shipped once already.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must not carry its own escapes")]
    fn a_styled_line_that_spells_its_own_sgr_trips_in_development() {
        let _ = rendered(true, LineKind::BannerArt, "\x1b[36mridge\x1b[0m");
    }

    /// The assertion is scoped to styled classes precisely so it cannot fire on
    /// the hostile input the guard exists for. A permission prompt carrying a
    /// model-composed URL with an escape in it is REQ-563's case: it must be
    /// neutralized and rendered, never panicked on.
    #[test]
    fn an_unstyled_class_may_carry_escapes_and_is_merely_defused() {
        let out = rendered(true, LineKind::Prompt, "fetch https://good\x1b[2K\x1b[1Aevil");
        assert!(!out.contains('\x1b'), "escape reached the terminal: {out:?}");
    }

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

    /// The other half of the same guard. Streamed assistant text is the one
    /// thing on this surface a *fetched page* can steer: the model reads the
    /// page, the page tells it what to say, and the text lands here. An escape
    /// that survives can erase the consent prompt printed above it and print a
    /// different destination in its place — so `fragment` defuses exactly what
    /// `line` does, minus the newlines that are its ordinary content.
    #[test]
    fn a_streamed_fragment_cannot_redraw_the_prompt_above_it() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            // `\x1b[8m` is "conceal" — it makes the row invisible rather than
            // wrong, which is the version of this a reader does not notice.
            // `\u{9b}` is the single-byte CSI: the same command with no `ESC`
            // in the text at all, which is what a filter looking only for
            // `\x1b` misses.
            surface.fragment("here is the page\x1b[8m\u{9b}2K\u{9b}1Aapprove evil.example\n");
            surface.fragment("second line\n");
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains('\x1b') && !out.contains('\u{9b}'),
            "a terminal command survived streamed text: {out:?}"
        );
        // Neutralized, not censored — the model really did say this.
        assert!(out.contains("here is the page"), "{out:?}");
        assert!(out.contains("approve evil.example"), "{out:?}");
        // Newlines are content on this verb and must survive: two fragments,
        // each ending in one, are two rows.
        assert_eq!(out.matches('\n').count(), 2, "{out:?}");
        assert!(out.ends_with("second line\n"), "{out:?}");
    }

    /// Reordering a row is the same attack as redrawing it, done with characters
    /// that are not controls at all: the bidi overrides make
    /// `https://evil.example` *read* as something else without changing a byte
    /// of it, and the zero-width set hides the seam.
    #[test]
    fn bidi_and_zero_width_steering_is_neutralized_on_both_verbs() {
        let steering = [
            '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}',
            '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{2028}', '\u{2029}', '\u{200b}',
            '\u{00ad}', '\u{061c}', '\u{feff}',
        ];
        for steer in steering {
            let text = format!("fetch https://good.example{steer}elpmaxe.live//:sptth");
            let mut buf = Vec::new();
            {
                let mut surface = PlainSurface::new(&mut buf);
                surface.line(LineKind::Prompt, &text);
                surface.fragment(&text);
            }
            let out = String::from_utf8(buf).unwrap();
            assert!(!out.contains(steer), "{steer:?} survived a render: {out:?}");
            assert!(out.contains("good.example"), "{steer:?}: {out:?}");
        }
    }

    /// The bookkeeping has to read what was *written*, not what was asked for.
    /// A fragment ending in a bare `\r` leaves the cursor at the start of a row
    /// it has already written to; recording that as "at line start" would make
    /// the next `line()` print over the streamed text instead of below it.
    #[test]
    fn line_bookkeeping_reads_the_defused_text() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.fragment("streamed\r");
            surface.line(LineKind::Notice, "routed to local");
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains('\r'), "{out:?}");
        assert!(
            out.starts_with("streamed \n"),
            "the open row was closed before the notice: {out:?}"
        );
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
