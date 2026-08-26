//! The rendering seam.
//!
//! Every character the CLI *renders* goes through a [`Surface`]. The one other
//! writer to the same terminal is a [`crate::prompt::Prompter`], which puts a
//! question on the row it is about to read from; it is not a `Surface`, so it
//! calls [`defused`] itself and the guard covers both writers rather than one
//! of them (REQ-573). The MVP ships one
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

use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::Range;

use crate::markdown::{self, Block, Inline, InlineStyle};

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

/// The SGR parameters each inline markdown run is drawn with (REQ-592 BR-5).
///
/// [`LineKind::sgr`]'s table again, for the other axis: that one keys on the
/// class of a whole line composed by this binary, this one keys on a run parsed
/// out of the **model's** text. The reason they are both here rather than at
/// their callers is identical and is the sharper of the two here — assistant
/// text is the one thing on this surface a fetched page can steer, and
/// [`defused_multiline`] has already replaced every `\x1b` in it with a space.
/// A renderer that let the text carry its own SGR would be handing that page the
/// cursor back, which is the hole REQ-563/573 closed; a renderer that read
/// markers and then *printed* them would be the defect REQ-592 exists to fix. So
/// the seam reads the markers, drops them, and authors the escape itself from
/// this fixed alphabet ([[LESSON-517]]).
///
/// Emphasis is italic rather than dim because emphasis is supposed to stand out;
/// a terminal that does not implement SGR 3 ignores it and the text is merely
/// unstyled, which is the same outcome as `NO_COLOR`. A code span takes the
/// banner art's cyan — a colour rather than an attribute, so a run of code stays
/// legible next to bold and italic prose.
fn inline_sgr(style: InlineStyle) -> &'static str {
    match style {
        InlineStyle::Strong => "1",
        InlineStyle::Emphasis => "3",
        InlineStyle::Code => "36",
    }
}

/// What a heading's whole row is drawn with. The `#` markers are not printed
/// (REQ-592's recognized-construct table), so with colour off a heading is its
/// own text and nothing else — the same trade `NO_COLOR` makes everywhere.
const HEADING_SGR: &str = "1";

/// Closes every attribute this surface opens. One reset ends a nested pair as
/// surely as it ends a single one, which is why the styled row never has to
/// track what it has to undo.
const RESET: &str = "\x1b[0m";

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

    /// Declare that the block of streamed output just ended: emit anything the
    /// surface is still holding, and drop whatever block state it accumulated
    /// while holding it (REQ-592 BR-8, ADR-3).
    ///
    /// **Deciding that a block has ended is the caller's knowledge, not the
    /// surface's** — the same division `repaint_row_above` makes about frame
    /// geometry. A streaming renderer cannot tell a pause in the token stream
    /// from the end of a reply, so it must never guess: the tail of a turn is
    /// emitted because the event pump *knows* the turn is over, never because a
    /// timer or a heuristic inside the surface decided the model had stopped
    /// talking.
    ///
    /// **Every call site of this verb lives in `client.rs`'s event pump**
    /// (ADR-3, [[LESSON-547]]). Not `main.rs`, not `hand_off_after_turn`, not a
    /// self-flush in this module. The obvious site is `hand_off_after_turn`, and
    /// it is wrong: only the `Ok` arm of the turn match reaches it, so a flush
    /// hung there would drop buffered text on every failed turn and miss the
    /// transport `?` entirely. The pump is the one place that owns the surface
    /// on every path an event can take — including the idle path, where
    /// fragments arrive with no turn in flight at all.
    ///
    /// **A turn boundary, and only a turn boundary.** This verb drops block
    /// state — the open-fence bit among it — because the *block* ended, so
    /// calling it at a mid-turn pause is a bug rather than a harmless extra
    /// flush. A model that opens a ` ```rust ` fence, hits a tool call, and
    /// resumes after the user answers the permission prompt would have the rest
    /// of its code classified as markdown: `**ptr` opens a strong run, `*y * z`
    /// picks up emphasis. That is BR-6's failure, caused by an over-eager call
    /// to the thing meant to prevent a different one. If a mid-turn caller needs
    /// the buffer on screen ahead of its own row, that is what `line()` and
    /// `repaint_row_above()` already do for themselves.
    ///
    /// **Defaults to a no-op**, for `repaint_row_above`'s reason: a surface that
    /// holds nothing has nothing to emit, so silence is the correct behaviour
    /// rather than something each implementor must remember to add. That default
    /// is also what keeps this verb from rippling through the ~15 modules that
    /// consume `&mut dyn Surface` and the three implementors that buffer
    /// nothing.
    fn end_block(&mut self) {}

    /// Flush any buffered output. The default is a no-op.
    ///
    /// # Errors
    ///
    /// Returns any error the underlying writer raises while flushing.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Everything the markdown renderer has to remember between calls (REQ-592
/// ADR-1). Held in an `Option` on the surface, so a surface without one is not a
/// surface with the renderer switched off — it has no renderer at all.
///
/// All three buffers exist because assistant text arrives token by token and
/// none of the decisions can be made from one chunk:
///
/// - `pending` — a line is the unit every layout decision is taken over, and no
///   single `fragment()` is a line. Text is held here until a `\n` completes it,
///   or until something else claims the row (BR-8) and forces the partial line
///   out as it stands.
/// - `table` — a table's columns cannot be measured until the run of rows ends,
///   which is the accepted cost in BR-4. The rows are held as their *source*
///   lines, because [`markdown::layout_table`] is what turns a run into display
///   text and it takes the whole run at once.
/// - `fence` — [`markdown::classify`] is deliberately line-oriented and holds no
///   memory, so the one bit of block state a streaming renderer needs lives
///   here. Inside a fence nothing is classified at all (BR-6).
///
/// **Nothing here is flushed on a timer or a heuristic.** The verb that empties
/// these at the end of a turn is `Surface::end_block`, and every call site of it
/// belongs to `client.rs`'s event pump (ADR-3) — this module must never decide
/// on its own that a block has ended.
struct MarkdownState {
    /// The terminal width to lay out at, in columns. A parameter, never a query:
    /// the surface is handed the answer by the wiring that knows there is a
    /// terminal at all (BR-7, BR-10).
    width: usize,
    /// Streamed text received since the last `\n`, already defused.
    pending: String,
    /// Consecutive table rows buffered until the run ends, as their source text.
    table: Vec<String>,
    /// Whether a ` ``` ` fence is currently open.
    fence: bool,
}

/// A plain streaming-text surface over any [`Write`] (stdout in the binary).
///
/// It tracks whether the cursor is at the start of a line so that a `line()`
/// arriving in the middle of streamed `fragment()`s first closes the open line —
/// keeping notices and assistant text from colliding on one row.
///
/// Since REQ-592 it optionally renders markdown, and the option is taken **at
/// construction** rather than tested inside `fragment()`. That is ADR-1, and the
/// consequence it buys is BR-7: the piped path builds a surface with no
/// renderer, so "inert off a terminal" is true by construction rather than by a
/// conditional a later edit could invert — and every test that builds one
/// through [`PlainSurface::new`] or [`PlainSurface::with_color`] keeps its bytes
/// unchanged without having to say so.
pub struct PlainSurface<W: Write> {
    out: W,
    at_line_start: bool,
    color: bool,
    /// The markdown renderer, or `None` for the raw pass-through path.
    markdown: Option<MarkdownState>,
}

impl<W: Write> PlainSurface<W> {
    /// Wraps `out` in a surface that emits no colour and renders no markdown.
    /// Starts assuming a fresh line.
    pub fn new(out: W) -> Self {
        Self::with_color(out, false)
    }

    /// Wraps `out` in a surface that draws styled line classes with SGR when
    /// `color`. Whether the target can take colour is a property of the target,
    /// so it is the surface that holds the answer — the callers composing lines
    /// never need to know.
    ///
    /// Renders no markdown: assistant text is passed through defused and
    /// otherwise untouched, which is what every non-terminal target wants.
    pub fn with_color(out: W, color: bool) -> Self {
        Self {
            out,
            at_line_start: true,
            color,
            markdown: None,
        }
    }

    /// Wraps `out` in a surface that renders assistant text as markdown at
    /// `width` columns, styling it when `color` (REQ-592 BR-3..BR-6).
    ///
    /// The third constructor rather than a flag on the second, because the two
    /// answers are independent and one of them is not about colour: a terminal
    /// under `NO_COLOR` still wants its prose wrapped and its tables laid out,
    /// it just wants none of it in SGR. `color` gates only the escapes.
    ///
    /// `width` is passed in because the query lives in `prompt.rs` and the
    /// decision to render at all lives in the wiring that knows stdout is a
    /// terminal — BR-10's rule, so that every layout decision below is reachable
    /// from a test with no terminal in sight.
    // The one caller outside this module's own tests is `main.rs`'s surface
    // construction, and it arrives in TASK-281 of this REQ — the task that owns
    // the terminal gate, and the only place that knows whether stdout is one
    // (ADR-1, [[LESSON-547]]: a rule that crosses a seam is owned by exactly one
    // side). Until then the release build has no caller for this.
    //
    // `expect` rather than `allow` on purpose. An `allow` would go on being
    // correct after TASK-281 wires this up and would sit here forever — which is
    // the lingering-suppression failure tetond's ADR-J is about, and the same
    // trap `markdown.rs`'s module-wide `allow(dead_code)` set for this task.
    // `expect` inverts it: the moment a real caller exists the lint stops firing
    // and *this attribute* becomes the warning, so `-D warnings` makes removing
    // it a condition of landing the wiring rather than a note someone has to
    // remember.
    //
    // Scoped to `not(test)` because the two compilations of this file disagree
    // about the fact being asserted: the test target has sixteen callers below
    // and the release binary has none, so an unconditional `expect` would be
    // unfulfilled — and therefore a warning — in exactly the build where the
    // constructor *is* exercised.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "TASK-281 adds the main.rs caller; this attribute fails the build once it does"
        )
    )]
    pub fn with_markdown(out: W, color: bool, width: usize) -> Self {
        Self {
            out,
            at_line_start: true,
            color,
            markdown: Some(MarkdownState {
                width,
                pending: String::new(),
                table: Vec::new(),
                fence: false,
            }),
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
///
/// `pub(crate)` since REQ-573: a [`crate::prompt::Prompter`] writes its question
/// straight to the terminal without going through a [`Surface`], and the question
/// can carry daemon-supplied text (the offered auth template, a tool name). That
/// writer needs *this* transform rather than a second one — one sanitizer, two
/// writers, or the two drift and the weaker one is the way in.
pub(crate) fn defused(text: &str) -> String {
    neutralized(text, false)
}

/// [`neutralized`] for streamed prose: newlines survive, everything else that
/// commands the terminal does not.
fn defused_multiline(text: &str) -> String {
    neutralized(text, true)
}

/// One wrapped row's bytes: the text of `span`, with the SGR this surface
/// authors drawn over the runs [`markdown::parse_inline`] found.
///
/// `base` is an attribute the *whole* row carries — a heading's bold — and it is
/// re-opened after every inner run's reset, because one `\x1b[0m` ends
/// everything that is open and there is no way to close just the inner one. An
/// inner run inside a base therefore opens as a combined `base;inner`, which is
/// how a code span inside a heading stays bold and cyan rather than losing the
/// bold at its own reset.
///
/// Called **only** on the coloured path. With colour off the surface authors no
/// escape at all — see [`PlainSurface::block_rows`], where that is a structural
/// property rather than a branch that happens to be empty.
fn styled_span(inline: &Inline, span: &Range<usize>, base: Option<&str>) -> String {
    let mut out = String::with_capacity(span.len());
    if let Some(base) = base {
        let _ = write!(out, "\x1b[{base}m");
    }
    // Walked one character at a time and asked per byte offset rather than
    // intersected span-by-span, because `style_at` is the accessor `Inline`
    // exposes for exactly this and asking it is what keeps the styling indexed
    // to the same string the break was measured from ([[LESSON-529]]).
    let mut open: Option<InlineStyle> = None;
    for (at, c) in inline.text[span.clone()].char_indices() {
        let here = inline.style_at(span.start + at);
        if here != open {
            if open.is_some() {
                out.push_str(RESET);
            }
            match (base, here) {
                (Some(base), Some(style)) => {
                    let _ = write!(out, "\x1b[{base};{}m", inline_sgr(style));
                }
                (None, Some(style)) => {
                    let _ = write!(out, "\x1b[{}m", inline_sgr(style));
                }
                // The base was reset alongside the run that just closed, so it
                // has to be re-opened for the plain text that follows.
                (Some(base), None) if open.is_some() => {
                    let _ = write!(out, "\x1b[{base}m");
                }
                (Some(_) | None, None) => {}
            }
            open = here;
        }
        out.push(c);
    }
    if open.is_some() || base.is_some() {
        out.push_str(RESET);
    }
    out
}

/// [`markdown::wrap_indented`]'s rows, assembled here so that each one can carry
/// SGR (REQ-592 BR-3 and BR-5 together).
///
/// The surface assembles rather than delegates on this path for one reason:
/// [`markdown::wrap_ranges`] returns **byte ranges** into the same string
/// [`markdown::parse_inline`] indexed its spans against, so a style lands on the
/// bytes the break was measured from. Taking the finished strings back instead
/// and finding the styled runs in them again would be a second measurement that
/// could disagree with the first, which is the whole shape of [[LESSON-529]].
fn styled_rows(
    inline: &Inline,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
    base: Option<&str>,
) -> Vec<String> {
    let first_avail = width.saturating_sub(markdown::display_width(first_prefix));
    let cont_avail = width.saturating_sub(markdown::display_width(cont_prefix));
    markdown::wrap_ranges(&inline.text, first_avail, cont_avail)
        .into_iter()
        .enumerate()
        .map(|(row, span)| {
            let prefix = if row == 0 { first_prefix } else { cont_prefix };
            format!("{prefix}{}", styled_span(inline, &span, base))
        })
        .collect()
}

/// The markdown renderer's half of [`PlainSurface`]. Every method here is inert
/// — an early `return` on a `None` field — when the surface was built without a
/// renderer, which is what makes BR-7 a property of construction (ADR-1).
impl<W: Write> PlainSurface<W> {
    /// The width to lay out at, or the no-terminal default when there is no
    /// renderer to ask. The fallback is unreachable from the paths that use it
    /// (they all check the renderer first) and is named rather than invented so
    /// that it cannot become a different number from the width query's own
    /// fallback.
    fn markdown_width(&self) -> usize {
        self.markdown
            .as_ref()
            .map_or(markdown::DEFAULT_WIDTH, |state| state.width)
    }

    /// Write one finished row and its newline.
    ///
    /// Every byte the renderer emits goes through here, which is why
    /// `at_line_start` is simply true afterwards: the renderer never leaves a
    /// partial row on screen, so the bookkeeping still reads the **emitted**
    /// text rather than the argument, exactly as [`Surface::fragment`]'s own
    /// assignment does.
    fn write_row(&mut self, row: &str) {
        let _ = writeln!(self.out, "{row}");
        self.at_line_start = true;
    }

    /// One block's rows, styled or not.
    ///
    /// The split is not an optimization and it is not a duplicate layout: with
    /// colour off the surface has **no escape to author**, so the layout
    /// module's own rows are already the finished bytes and it writes them
    /// unchanged. That is what makes AC-8's "zero `\x1b` bytes" a property of
    /// the code path rather than of a table lookup that happens to return
    /// nothing — the uncoloured path never touches [`inline_sgr`] at all.
    ///
    /// Both arms bottom out in the same [`markdown::wrap_ranges`] call with the
    /// same two available widths, so they cannot disagree about where a row
    /// ends; and the prefixes each arm needs are stated once, in `markdown.rs`
    /// ([`markdown::list_item_prefixes`], [`markdown::QUOTE_PREFIX`]), so they
    /// cannot disagree about what a row starts with either.
    fn block_rows(&self, block: &Block<'_>) -> Vec<String> {
        let width = self.markdown_width();
        match block {
            Block::Heading { text, .. } => {
                let inline = markdown::parse_inline(text);
                if self.color {
                    styled_rows(&inline, width, "", "", Some(HEADING_SGR))
                } else {
                    markdown::wrap(&inline.text, width)
                }
            }
            Block::ListItem { marker, text } => {
                let inline = markdown::parse_inline(text);
                if self.color {
                    let (first, cont) = markdown::list_item_prefixes(marker);
                    styled_rows(&inline, width, &first, &cont, None)
                } else {
                    markdown::wrap_list_item(marker, &inline.text, width)
                }
            }
            Block::Quote { text } => {
                let inline = markdown::parse_inline(text);
                if self.color {
                    let quote = markdown::QUOTE_PREFIX;
                    styled_rows(&inline, width, quote, quote, None)
                } else {
                    markdown::wrap_block_quote(&inline.text, width)
                }
            }
            Block::Paragraph { indent, text } => {
                let inline = markdown::parse_inline(text);
                // The indent is a column count, so it is redrawn as spaces: a
                // tab that measured eight columns comes back as eight of them.
                // Keeping it at all is what makes an indented code block
                // legible-but-unstyled rather than silently un-indented (AC-14).
                let pad = " ".repeat(*indent);
                if self.color {
                    styled_rows(&inline, width, &pad, &pad, None)
                } else {
                    markdown::wrap_indented(&inline.text, width, &pad, &pad)
                }
            }
            // Structure, not prose. `Blank` and `ThematicBreak` are one fixed
            // row each; the fence and table variants are state the caller in
            // `render_source_line` handles before it ever gets here.
            Block::Blank
            | Block::ThematicBreak
            | Block::Fence { .. }
            | Block::TableRow { .. }
            | Block::TableSeparator { .. } => Vec::new(),
        }
    }

    /// Emit one classified block.
    fn render_block(&mut self, block: &Block<'_>) {
        match block {
            Block::Blank => self.write_row(""),
            Block::ThematicBreak => {
                let rule = markdown::thematic_break(self.markdown_width());
                self.write_row(&rule);
            }
            Block::Fence { .. } | Block::TableRow { .. } | Block::TableSeparator { .. } => {}
            Block::Heading { .. }
            | Block::ListItem { .. }
            | Block::Quote { .. }
            | Block::Paragraph { .. } => {
                let rows = self.block_rows(block);
                if rows.is_empty() {
                    // A construct with no text left after its markers — `# ` on
                    // its own, or a bare `>`. It occupied a row in the model's
                    // output, and emitting nothing would close up a paragraph
                    // break the reader was shown. The marker is not reprinted
                    // (that is the construct's whole point), so what lands is an
                    // empty row, or `>` for a quote.
                    let empty = match block {
                        Block::Quote { .. } => markdown::QUOTE_PREFIX.trim_end(),
                        _ => "",
                    };
                    self.write_row(empty);
                    return;
                }
                for row in rows {
                    self.write_row(&row);
                }
            }
        }
    }

    /// Lay out and emit the buffered table run, if there is one (BR-4).
    fn flush_table_run(&mut self) {
        let Some(state) = self.markdown.as_mut() else {
            return;
        };
        if state.table.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut state.table);
        let width = state.width;
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        // Emitted exactly as `layout_table` returned them. It hands back final
        // display text with the inline markers already removed and the padding
        // computed from the stripped widths, so a `parse_inline` pass here would
        // strip a second time and walk every cell four columns left per marker
        // pair — the table's own doc comment calls that out as a contract rather
        // than a detail. The recorded cost is BR-5's: no inline styling inside a
        // table cell, unstyled at the right column beating bold at the wrong one.
        for row in markdown::layout_table(&borrowed, width) {
            self.write_row(&row);
        }
    }

    /// Render one **complete** source line of assistant text.
    ///
    /// The fence check comes first and does not go through
    /// [`markdown::classify`] at all: BR-6 makes fence content verbatim, so
    /// classifying a line of shell inside one would read a glob's `*` as
    /// emphasis and a row of tabular output as a table cell. The closing
    /// delimiter is still recognized, through the same
    /// [`markdown::fence_delimiter`] the classifier itself asks — a fence the
    /// two disagreed about would never close, and every remaining line of the
    /// reply would render as code.
    fn render_source_line(&mut self, line: &str) {
        if self.markdown.as_ref().is_some_and(|state| state.fence) {
            if markdown::fence_delimiter(line).is_some() {
                self.set_fence(false);
            } else {
                self.write_row(line);
            }
            return;
        }

        match markdown::classify(line) {
            // A run of rows is buffered until something that is not a row ends
            // it, because a column's width is not knowable from one row.
            Block::TableRow { .. } | Block::TableSeparator { .. } => {
                if let Some(state) = self.markdown.as_mut() {
                    state.table.push(line.to_owned());
                }
            }
            Block::Fence { .. } => {
                self.flush_table_run();
                self.set_fence(true);
            }
            other => {
                self.flush_table_run();
                self.render_block(&other);
            }
        }
    }

    /// Open or close the fence bit.
    fn set_fence(&mut self, open: bool) {
        if let Some(state) = self.markdown.as_mut() {
            state.fence = open;
        }
    }

    /// Emit everything the renderer is still holding, so that a caller about to
    /// claim a row does not paint over text the reader has not seen (BR-8).
    ///
    /// Order matters and is not the order the buffers are declared in. The
    /// partial line goes **first**, because it is the newest text in the stream
    /// and it may itself be the last row of the open table run — closing the run
    /// before classifying it would split one table into two.
    ///
    /// This is not `end_block()`. That verb and every one of its call sites
    /// belong to `client.rs`'s event pump (ADR-3); what happens here is the
    /// narrow case where `line()` or `repaint_row_above()` is about to write and
    /// the buffer must go out ahead of it.
    fn emit_pending(&mut self) {
        let Some(state) = self.markdown.as_mut() else {
            return;
        };
        let pending = std::mem::take(&mut state.pending);
        if !pending.is_empty() {
            self.render_source_line(&pending);
        }
        self.flush_table_run();
    }
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

        // A line owns its row, so anything the renderer is still holding goes
        // out ahead of it (REQ-592 BR-8) — otherwise a notice arriving mid-turn
        // prints above a sentence the reader has not been shown yet, and the
        // screen reads in the wrong order. Inert without a renderer, and it
        // leaves the surface at the start of a row, so the close below is
        // unchanged for both paths.
        self.emit_pending();

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
    ///
    /// With a renderer attached (REQ-592) the defusing happens **first and
    /// unchanged**, and every markdown decision is taken over the already-defused
    /// text. That ordering is the feature's central constraint: a renderer that
    /// parsed first and defused second would be reading an attacker's escape
    /// bytes as markup, and one that let its own styling through the guard would
    /// have to weaken the guard. Here the escapes are already spaces by the time
    /// [`markdown::classify`] sees the line, and the SGR is authored afterwards
    /// from [`inline_sgr`]'s fixed table ([[LESSON-517]], BR-5).
    fn fragment(&mut self, text: &str) {
        let shown = defused_multiline(text);
        if self.markdown.is_none() {
            let _ = write!(self.out, "{shown}");
            self.at_line_start = shown.ends_with('\n');
            return;
        }

        // A line is the unit every layout decision is taken over and no single
        // chunk is a line, so the text accumulates until a `\n` completes one.
        // What is left over stays held: the tail of a turn is emitted by
        // `end_block()` from the event pump (ADR-3), never by this module
        // guessing that the model has stopped talking.
        let mut complete: Vec<String> = Vec::new();
        if let Some(state) = self.markdown.as_mut() {
            state.pending.push_str(&shown);
            while let Some(at) = state.pending.find('\n') {
                let rest = state.pending.split_off(at + 1);
                let mut line = std::mem::replace(&mut state.pending, rest);
                line.pop();
                complete.push(line);
            }
        }
        for line in complete {
            self.render_source_line(&line);
        }
    }

    /// Save the cursor, step up, clear that row, write, restore. `at_line_start`
    /// is deliberately untouched: the cursor ends where it began, so the
    /// bookkeeping that keeps a later `line()` from colliding with streamed
    /// output is still accurate.
    fn repaint_row_above(&mut self, rows_up: usize, kind: LineKind, text: &str) {
        // Same rule as `line()`, for the same reason (BR-8): a repaint moves the
        // cursor over rows that are already on screen, and buffered text is text
        // that is not on screen yet. Emitting first also keeps `rows_up`
        // measuring from the row the caller believes it is measuring from —
        // scrolling the frame *after* the offset was chosen is what would put
        // the indicator somewhere else.
        self.emit_pending();

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

    /// Emit the held tail and forget the block state that produced it.
    ///
    /// Two halves, and the second is the one that is easy to leave out. The
    /// buffers go out through the same [`Self::emit_pending`] a mid-stream
    /// `line()` uses, in the same order and for the same reason. **Then the
    /// fence bit is cleared**, which no other path in this module ever does: a
    /// reply that opened a ` ``` ` and never closed it leaves `fence == true`,
    /// and a bit that survives the turn makes every subsequent line of every
    /// subsequent turn render verbatim — no wrap, no styling, for the rest of
    /// the session. The renderer cannot clear it on its own, because inside a
    /// fence "this line is not markup" is exactly what it is supposed to
    /// believe; only a caller that knows the block is over can say otherwise,
    /// which is what this verb is.
    ///
    /// Order matters: the tail is emitted **before** the bit is dropped, so a
    /// partial last line inside a fence still goes out verbatim rather than
    /// being classified on its way past a fence that had just been declared
    /// shut.
    ///
    /// That the bit is dropped at all is why the trait's contract restricts this
    /// verb to a turn boundary — see [`Surface::end_block`].
    fn end_block(&mut self) {
        self.emit_pending();
        self.set_fence(false);
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
            assert_eq!(
                out.trim_end_matches('\n').rfind("\x1b["),
                out.rfind("\x1b[0m")
            );
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
        let out = rendered(
            true,
            LineKind::Prompt,
            "fetch https://good\x1b[2K\x1b[1Aevil",
        );
        assert!(
            !out.contains('\x1b'),
            "escape reached the terminal: {out:?}"
        );
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

    // ---- REQ-592: the markdown renderer -----------------------------------
    //
    // Every test below opts in through `with_markdown`. Every test *above*
    // builds through `new`/`with_color` and is unchanged by this REQ, which is
    // ADR-1's whole claim: the renderer is a third constructor, not a branch
    // inside the two that already shipped.

    /// Drive a markdown surface with a scripted sequence of streamed chunks.
    ///
    /// There is no flush here on purpose. `end_block()` and its call sites are
    /// TASK-280's, and this module must never decide on its own that the model
    /// has stopped talking — so a chunk sequence that does not end in a `\n`
    /// leaves its tail held, and these tests say so where it matters.
    fn markdown_out(color: bool, width: usize, chunks: &[&str]) -> String {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, color, width);
            for chunk in chunks {
                surface.fragment(chunk);
            }
        }
        String::from_utf8(buf).unwrap()
    }

    /// [`markdown_out`] with the block declared over afterwards — what
    /// `client.rs`'s pump does at the end of a turn (ADR-3).
    ///
    /// Deliberately identical to `markdown_out` but for the one extra call, so a
    /// pair of assertions taken over both is a statement about `end_block` and
    /// nothing else.
    fn markdown_out_ended(color: bool, width: usize, chunks: &[&str]) -> String {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, color, width);
            for chunk in chunks {
                surface.fragment(chunk);
            }
            surface.end_block();
        }
        String::from_utf8(buf).unwrap()
    }

    /// Everything that is not whitespace, in order — what a construct's
    /// characters are, independent of where the rows were broken.
    fn ink(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// **AC-5.** The guard and the styling in the same chunk, which is the only
    /// arrangement that proves which direction the bytes flow.
    ///
    /// A fetched page can steer assistant text (REQ-563), so an `\x1b[2K\x1b[1A`
    /// arriving mid-sentence must reach the terminal as the visible characters
    /// the page really contained — while `**bold**` in the *same* chunk still
    /// comes out as a real SGR sequence. The two together are only possible if
    /// the escape is authored here, after [`defused_multiline`], from
    /// [`inline_sgr`]'s table: a renderer that passed the model's escapes
    /// through would emit both, and one that let markdown style itself would
    /// have had to stop defusing to do it ([[LESSON-517]]).
    ///
    /// Mutation-checked: drop the `defused_multiline` call in `fragment` and the
    /// first two assertions fail on the cursor-motion sequences.
    #[test]
    fn a_rendered_fragment_defuses_escapes_and_still_authors_its_own_sgr() {
        let out = markdown_out(true, 60, &["here is \x1b[2K\x1b[1A**bold** text\n"]);

        assert!(
            !out.contains("\x1b[2K"),
            "an erase-line command survived the renderer: {out:?}"
        );
        assert!(
            !out.contains("\x1b[1A"),
            "a cursor-up command survived the renderer: {out:?}"
        );
        // Neutralized, not censored: the ESC became a space and the rest of the
        // sequence is ordinary text, because the page really did contain it.
        assert!(
            out.contains("[2K"),
            "the escape's remains are shown: {out:?}"
        );
        // Authored here, from the fixed table, over text that has no escapes
        // left in it.
        assert!(
            out.contains("\x1b[1mbold\x1b[0m"),
            "the strong run was not styled by the surface: {out:?}"
        );
        assert!(
            !out.contains("**"),
            "the markers were printed instead of drawn: {out:?}"
        );
    }

    /// **AC-8, unit leg.** With colour off the surface authors no escape at all
    /// — not an empty one, not a reset. The rows are still wrapped, because
    /// wrapping is not styling: a terminal under `NO_COLOR` has exactly the same
    /// width problem as one without it.
    #[test]
    fn an_uncolored_markdown_surface_wraps_and_emits_no_escapes() {
        let source = "**strong** and *emphasis* and `code` in a paragraph long enough to wrap\n";

        let plain = markdown_out(false, 20, &[source]);
        assert!(
            !plain.contains('\x1b'),
            "colour is off and an escape was authored anyway: {plain:?}"
        );
        // Markers are still consumed — printing `**strong**` verbatim is the
        // defect, and it is a defect at every colour setting.
        assert!(!plain.contains("**") && !plain.contains('`'), "{plain:?}");
        assert!(plain.contains("strong"), "{plain:?}");
        for row in plain.lines() {
            assert!(
                markdown::display_width(row) <= 20,
                "a row exceeded the width: {row:?}"
            );
        }

        // The same input with colour on carries the AC-5 alphabet and nothing
        // else.
        let styled = markdown_out(true, 20, &[source]);
        assert!(styled.contains("\x1b[1mstrong\x1b[0m"), "{styled:?}");
        assert!(styled.contains("\x1b[3memphasis\x1b[0m"), "{styled:?}");
        assert!(styled.contains("\x1b[36mcode\x1b[0m"), "{styled:?}");
    }

    /// The two arms of `block_rows` — the surface assembling a styled row, and
    /// `markdown.rs` returning an unstyled one — must agree about where a row
    /// starts and where it ends. They share the wrap, but each builds its own
    /// prefixes, so this is the assertion that keeps the marker and the hanging
    /// indent from drifting apart.
    #[test]
    fn the_styled_and_unstyled_paths_lay_a_row_out_identically() {
        for source in [
            "a paragraph with no inline markers at all, long enough to wrap twice over\n",
            "- a list item with no markers, long enough that it wraps under its own text\n",
            "> a quoted line with no markers, long enough to need a second quoted row\n",
            "    an indented line, which is a paragraph carrying its indentation\n",
        ] {
            assert_eq!(
                markdown_out(true, 24, &[source]),
                markdown_out(false, 24, &[source]),
                "styled and unstyled disagreed on a line with nothing to style: {source:?}"
            );
        }
    }

    /// **AC-6.** Fence content is verbatim: original line breaks, no wrapping
    /// even past the width, no styling, and the fence markers themselves are not
    /// printed. A wrapped line of code is a wrong line of code, and a `*` in a
    /// shell glob is not emphasis.
    #[test]
    fn fenced_code_is_verbatim_and_its_markers_are_not_printed() {
        let code = "for f in *.rs; do echo \"$f\"; done  # **not bold**, and `not code`";
        let out = markdown_out(
            true,
            20,
            &["```sh\n", code, "\n", "```\n", "after the fence\n"],
        );

        assert!(
            !out.contains("```"),
            "the fence markers were printed: {out:?}"
        );
        assert!(
            out.contains(&format!("{code}\n")),
            "the code line was reflowed or restyled: {out:?}"
        );
        assert!(
            markdown::display_width(code) > 20,
            "the fixture stopped being wider than the width, so this proves nothing"
        );
        // Nothing inside the fence was styled — the only thing that could have
        // authored an escape here is the inline table, and BR-6 turns it off.
        assert!(!out.contains('\x1b'), "a fenced block was styled: {out:?}");
        // The fence closed: the line after it is prose again, wrapped at 20.
        assert!(out.ends_with("after the fence\n"), "{out:?}");
    }

    /// **BR-4.** A run of table rows is held until something that is not a row
    /// ends it, then laid out as a block — columns lined up, the separator drawn
    /// as a rule rather than printed, and the pipes gone.
    ///
    /// The first assertion is the buffering itself, which is BR-4's accepted
    /// cost: a column's width is not knowable from one row, so nothing can be
    /// emitted until the run is complete.
    #[test]
    fn a_table_run_is_buffered_until_it_ends_and_then_laid_out() {
        let rows = [
            "| Surface | Finding |\n",
            "|---------|---------|\n",
            "| render  | wraps   |\n",
            "| prompt  | asks    |\n",
        ];

        let held = markdown_out(false, 40, &rows);
        assert_eq!(
            held, "",
            "a table row reached the terminal before its run ended, so its \
             column widths were measured against part of the table"
        );

        // A blank line is not a table row, so the run ends and the block is laid
        // out.
        let mut ended = rows.to_vec();
        ended.push("\n");
        assert_eq!(
            markdown_out(false, 40, &ended),
            "Surface  Finding\n\
             ────────────────\n\
             render   wraps\n\
             prompt   asks\n\
             \n"
        );
    }

    /// The pending partial line is emitted **before** the table run is closed,
    /// not after — it is the newest text in the stream, and it may itself be the
    /// run's last row. Getting the order wrong splits one table into two, which
    /// re-measures every column against half the rows.
    #[test]
    fn a_partial_last_row_joins_its_table_rather_than_starting_a_second_one() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 40);
            surface.fragment("| a | b |\n|---|---|\n");
            // No trailing newline: this row is still the pending partial line
            // when the notice forces the buffer out.
            surface.fragment("| ccc | ddd |");
            surface.line(LineKind::Notice, "routed to local");
        }
        let out = String::from_utf8(buf).unwrap();

        // Columns measured across all three rows: three columns wide, not one.
        // A split run would have laid the header out at width 1 and produced
        // `a  b`.
        assert_eq!(
            out,
            "a    b\n\
             ────────\n\
             ccc  ddd\n\
             >> routed to local\n"
        );
    }

    /// **BR-5's recorded limitation.** `layout_table` returns final display text
    /// with the markers already removed and the padding computed from the
    /// stripped widths, so the surface emits its rows untouched. A bold cell is
    /// therefore unstyled at the right column rather than bold at the wrong one
    /// — a second `parse_inline` pass here would walk every cell four columns
    /// left per marker pair.
    #[test]
    fn a_table_cell_is_not_styled_and_is_not_shifted() {
        let out = markdown_out(
            true,
            40,
            &["| **aa** | b |\n", "|---|---|\n", "| cc | dd |\n", "\n"],
        );
        assert!(
            !out.contains('\x1b'),
            "a table cell was styled, which un-aligns the column it sits in: {out:?}"
        );
        assert!(!out.contains("**"), "the markers were printed: {out:?}");
        assert_eq!(
            out,
            "aa  b\n\
             ──────\n\
             cc  dd\n\
             \n"
        );
    }

    /// **AC-9.** A notice arriving mid-stream emits *after* the pending buffer,
    /// not through it: the streamed sentence is complete on its own row and the
    /// notice starts clean below it. Held text is text the reader has not been
    /// shown, and a notice printed over it puts the screen in the wrong order.
    #[test]
    fn a_line_emits_the_pending_buffer_before_claiming_its_row() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
            surface.fragment("the finding is that the guard ");
            surface.line(LineKind::Notice, "routed to local");
            surface.fragment("holds.\n");
        }
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out,
            "the finding is that the guard\n>> routed to local\nholds.\n"
        );
    }

    /// **AC-9, semantic leg.** The same ordering seen as `(kind, text)` pairs.
    /// A `RecordingSurface` has no renderer and no buffer — which is the point:
    /// BR-9's ordering is a property of the call sequence, so it must read the
    /// same on a surface that transforms nothing.
    #[test]
    fn the_recorded_order_puts_a_mid_stream_notice_after_the_text_before_it() {
        let mut surface = RecordingSurface::new();
        surface.fragment("the finding is that the guard ");
        surface.line(LineKind::Notice, "routed to local");
        surface.fragment("holds.\n");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Fragment("the finding is that the guard ".to_owned()),
                Rendered::Line(LineKind::Notice, "routed to local".to_owned()),
                Rendered::Fragment("holds.\n".to_owned()),
            ]
        );
    }

    /// A repaint moves the cursor over rows that are already on screen, so
    /// buffered text — which is not on screen — goes out ahead of it (BR-8).
    /// Emitting first is also what keeps `rows_up` measuring from the row the
    /// caller chose it against.
    #[test]
    fn a_repaint_emits_the_pending_buffer_before_moving_the_cursor() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
            surface.fragment("partially streamed");
            surface.repaint_row_above(2, LineKind::Notice, "model starting..");
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.starts_with("partially streamed\n\x1b[s"),
            "the buffered row must be on screen before the cursor is saved: {out:?}"
        );
    }

    /// The buffer is held until a `\n` completes the line, and **nothing in this
    /// module ends it early**. No timer, no heuristic, no "the chunk looked
    /// finished". The verb that empties the tail at end of turn is
    /// `end_block()`, and every call site of it belongs to `client.rs`'s event
    /// pump (ADR-3) — so a partial line with nothing after it stays held here,
    /// deliberately.
    #[test]
    fn a_partial_line_is_held_until_a_newline_completes_it() {
        assert_eq!(markdown_out(false, 40, &["half a "]), "");
        assert_eq!(
            markdown_out(false, 40, &["half a ", "sentence\n"]),
            "half a sentence\n"
        );
    }

    /// **AC-10, surface leg.** …and `end_block()` is what lets it go. A model
    /// whose last chunk carries no `\n` is the common case, not the exotic one,
    /// so "held forever" and "shown" differ by exactly this call.
    ///
    /// The two assertions differ only in that call — everything else about the
    /// two fixtures is identical — which is what makes this a statement about
    /// the verb rather than about the renderer.
    #[test]
    fn end_block_emits_a_tail_that_no_newline_ever_completed() {
        assert_eq!(
            markdown_out(false, 40, &["half a sentence"]),
            "",
            "without the verb the tail is held, deliberately"
        );
        assert_eq!(
            markdown_out_ended(false, 40, &["half a sentence"]),
            "half a sentence\n"
        );
    }

    /// **AC-10, table leg (BR-4).** A run of rows is buffered until something
    /// that is not a row ends it — and at the end of a turn, nothing does.
    /// `end_block()` closes the run and lays it out, and it does so *after* the
    /// held partial line has been classified, so the last row is part of the
    /// same table rather than the start of a second one.
    #[test]
    fn end_block_closes_a_table_run_whose_last_row_is_still_pending() {
        assert_eq!(
            markdown_out(false, 40, &["| a | b |\n|---|---|\n", "| ccc | ddd |"]),
            "",
            "the whole run is still buffered while the turn is running"
        );
        assert_eq!(
            markdown_out_ended(false, 40, &["| a | b |\n|---|---|\n", "| ccc | ddd |"]),
            "a    b\n\
             ────────\n\
             ccc  ddd\n",
            "columns measured across all three rows: a split run would have laid \
             the header out at width 1"
        );
    }

    /// **AC-10's fence clause, and the sharpest reason this verb exists.**
    ///
    /// A reply that opens a ` ``` ` and never closes it — a truncated answer, an
    /// interrupted turn, a model that simply forgot — leaves `fence == true`.
    /// Nothing else in this module clears it, on purpose: inside a fence "this
    /// line is not markup" is exactly what the renderer is supposed to believe,
    /// so it cannot decide on its own that the block is over. Without
    /// `end_block()` clearing it, every subsequent line of every subsequent turn
    /// renders verbatim — no wrap, no styling — for the rest of the session.
    ///
    /// Asserted across two turns on **one** surface, because a per-turn surface
    /// would clear the bit by construction and prove nothing.
    #[test]
    fn an_unterminated_fence_does_not_swallow_the_next_turn() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 20);
            // Turn one: a fence opens and the turn ends inside it.
            surface.fragment("```sh\ncargo test\n");
            surface.end_block();
            // Turn two: ordinary prose, wide enough to need wrapping — which is
            // precisely what a surviving fence bit would suppress.
            surface.fragment("alpha bravo charlie delta echo\n");
            surface.end_block();
        }
        let out = String::from_utf8(buf).unwrap();

        assert_eq!(
            out,
            "cargo test\n\
             alpha bravo charlie\n\
             delta echo\n",
            "the second turn rendered verbatim, so the first turn's unterminated \
             fence outlived it: {out:?}"
        );
    }

    /// **BR-6, across a mid-turn interruption.** A permission prompt, a routing
    /// notice, or an indicator repaint is a *pause* in a turn, not the end of
    /// one — so the buffered tail goes out ahead of the interrupting row (BR-8)
    /// and the fence bit **survives it**. Only `end_block()` drops that bit, and
    /// only the event pump calls `end_block()`, at a turn boundary.
    ///
    /// Without this, code the model resumes after the prompt is classified as
    /// markdown and **word-wrapped at the terminal width**, so one statement is
    /// broken across three rows mid-token. That is the renderer mangling the one
    /// thing BR-6 makes verbatim, and re-indenting a paste is the least of what
    /// it costs — a wrapped shell command is a *different command*.
    ///
    /// REQ-592's architecture originally put an `end_block()` call site
    /// immediately before `resolve_permission` (ADR-3 site 3, for ADR-4's
    /// ordering property). It was dropped for exactly this — and because it
    /// changed no bytes, the ordering already being guaranteed by
    /// `resolve_permission` rendering through `line()`.
    ///
    /// The fixture is chosen to be *destroyed* by a cleared fence rather than
    /// merely nudged by one: the resumed line is three times the width, so a
    /// classified copy is unmistakably re-flowed rather than coincidentally
    /// identical. (Verified by mutation — routing `line()` through `end_block()`
    /// yields `let b = *p * *q; //\na deliberately long\ntrailing comment`.)
    #[test]
    fn a_mid_turn_interruption_emits_the_tail_but_does_not_end_the_fence() {
        const RESUMED: &str = "let b = *p * *q; // a deliberately long trailing comment";
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, true, 20);
            surface.fragment("```rust\nlet a = 1;\n");
            // The interruption: a notice claims the row mid-fence, exactly as
            // `resolve_permission` does through `line()` before it asks.
            surface.line(LineKind::Notice, "permission requested: shell");
            // The model resumes *inside* the fence.
            surface.fragment(&format!("{RESUMED}\n"));
            surface.end_block();
        }
        let out = String::from_utf8(buf).unwrap();

        // The notice landed between the two code rows, not on top of either.
        assert_eq!(
            out,
            format!("let a = 1;\n>> permission requested: shell\n{RESUMED}\n"),
            "the fence did not survive the interruption"
        );
        // Colour is on, so an escape could only have been authored by the inline
        // styling table — which BR-6 turns off inside a fence.
        assert!(!out.contains('\x1b'), "a fenced line was styled: {out:?}");
        assert!(
            !out.contains("```"),
            "the fence marker was printed: {out:?}"
        );
    }

    /// **BR-7.** The piped path builds a surface with no renderer at all, and
    /// the new verb must be as inert there as `flush` is — including the
    /// newline it would otherwise add to a tail that never had one.
    #[test]
    fn end_block_writes_nothing_on_a_surface_with_no_renderer() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut buf);
            surface.fragment("a tail with no newline");
            surface.end_block();
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "a tail with no newline");
    }

    /// **BR-3 at the seam.** Breaks land on whitespace, never inside a word —
    /// `defens-\ne-in-depth` is the defect this REQ exists to remove — and a
    /// list item's continuation rows align under its text rather than under its
    /// marker.
    #[test]
    fn prose_is_wrapped_at_word_boundaries_with_the_right_hanging_indent() {
        assert_eq!(
            markdown_out(false, 20, &["alpha bravo charlie delta echo\n"]),
            "alpha bravo charlie\ndelta echo\n"
        );
        assert_eq!(
            markdown_out(
                false,
                20,
                &["- alpha bravo charlie delta echo foxtrot golf\n"]
            ),
            "- alpha bravo\n  charlie delta echo\n  foxtrot golf\n"
        );
        assert_eq!(
            markdown_out(false, 20, &["10. alpha bravo charlie delta echo\n"]),
            // Four columns of marker, so the continuation rows carry sixteen —
            // a wide marker narrows the text rather than pushing the row past
            // the edge.
            "10. alpha bravo\n    charlie delta\n    echo\n"
        );
        // The quote marker is on every row: a continuation row without it reads
        // as unquoted prose, which misattributes who said it.
        assert_eq!(
            markdown_out(false, 20, &["> alpha bravo charlie delta echo\n"]),
            "> alpha bravo\n> charlie delta echo\n"
        );
        // A word wider than the terminal is emitted whole and over-wide rather
        // than cut: a clipped row is a lie, and this is a security finding's
        // sentence.
        let long = "supercalifragilisticexpialidocious";
        let out = markdown_out(false, 20, &[&format!("a {long} b\n")]);
        assert!(out.contains(&format!("{long}\n")), "{out:?}");
    }

    /// Headings lose their `#` markers and gain the surface's own bold; a code
    /// span inside one opens as a **combined** `bold;cyan` and re-opens the bold
    /// after its reset, because one `\x1b[0m` ends everything that is open.
    #[test]
    fn a_heading_drops_its_markers_and_carries_the_surfaces_own_emphasis() {
        assert_eq!(
            markdown_out(false, 40, &["## Findings\n"]),
            "Findings\n",
            "the markers must not be printed at any colour setting"
        );
        assert_eq!(
            markdown_out(true, 40, &["## Findings\n"]),
            "\x1b[1mFindings\x1b[0m\n"
        );
        assert_eq!(
            markdown_out(true, 40, &["# The `Surface` seam\n"]),
            "\x1b[1mThe \x1b[1;36mSurface\x1b[0m\x1b[1m seam\x1b[0m\n"
        );
    }

    /// Blank lines are paragraph separation and are never collapsed; a thematic
    /// break is drawn as a rule at the width rather than printed as three
    /// dashes.
    #[test]
    fn blank_lines_survive_and_a_thematic_break_is_drawn() {
        assert_eq!(
            markdown_out(false, 8, &["one\n", "\n", "\n", "---\n", "two\n"]),
            "one\n\n\n────────\ntwo\n"
        );
    }

    /// **AC-14, surface leg.** Every construct REQ-592 puts out of scope reaches
    /// the terminal as literal text: no panic, no dropped characters, and no
    /// partial styling. This is the mitigation the hand-rolled parser rests on
    /// (OQ-2), so it is asserted rather than assumed — an unrecognized construct
    /// that swallowed content would make that decision wrong.
    #[test]
    fn every_out_of_scope_construct_reaches_the_terminal_as_literal_text() {
        for source in [
            // A nested list: the indented line is a paragraph carrying its
            // indent, by the same column-zero rule that makes an indented code
            // block literal.
            "- outer item\n  - inner item\n",
            // A setext heading's `=====` underline. (The `-----` form is a
            // thematic break by the recognized-construct table's own rule —
            // recorded in AC-14's carve-out and asserted separately below.)
            "Heading text\n=====\n",
            // An indented code block.
            "    let x = 1;\n",
            // Nested emphasis: literal from the opening marker to the closing
            // one, so no half of it is styled.
            "**bold with *italic* inside**\n",
        ] {
            let plain = markdown_out(false, 40, &[source]);
            assert_eq!(
                ink(&plain),
                ink(source),
                "characters were dropped or invented rendering {source:?}: {plain:?}"
            );
            let styled = markdown_out(true, 40, &[source]);
            assert!(
                !styled.contains('\x1b'),
                "an out-of-scope construct was partially styled: {styled:?}"
            );
        }

        // The fifth construct — a `|` inside a code span inside a table cell —
        // needs its own assertion rather than the sweep above, and the reason is
        // worth stating. What is out of scope is reading it as a **table**, and
        // that is exactly what does not happen: the classifier sees an odd
        // backtick count in a cell, refuses the row, and it falls through to a
        // paragraph. As a paragraph its code span is an ordinary code span, so
        // the backticks are consumed by a construct that *is* recognized. That
        // is not a dropped character in AC-14's sense — by that reading every
        // `**bold**` would be one — and the pipes, which are what the cell
        // boundary hazard is about, all survive as text.
        assert_eq!(
            markdown_out(false, 40, &["| `a|b` | c |\n"]),
            "| a|b | c |\n",
            "the row must stay prose: no rule, no column padding, every pipe intact"
        );
        assert_eq!(
            markdown_out(true, 40, &["| `a|b` | c |\n"]),
            "| \x1b[36ma|b\x1b[0m | c |\n",
            "the code span is styled whole or not at all — never half of it"
        );

        // The carve-out, asserted rather than left to be discovered: a line of
        // dashes is a thematic break here and in CommonMark alike, and a
        // line-oriented streaming classifier has no lookahead to read it as a
        // setext underline instead. The heading's *text* is untouched, so what
        // lands is text followed by a rule — which reads as an underline.
        assert_eq!(
            markdown_out(false, 5, &["Heading text\n-----\n"]),
            "Heading\ntext\n─────\n"
        );
    }

    /// **BR-7, structurally.** The two constructors that shipped before REQ-592
    /// attach no renderer, so a surface built through either one is byte-for-byte
    /// what it was: markdown intact, nothing wrapped, nothing buffered. This is
    /// why `cli_e2e`'s piped assertions do not move, and it is a property of
    /// construction rather than of a conditional a later edit could invert.
    #[test]
    fn the_pre_existing_constructors_attach_no_renderer() {
        let source = "| a | b |\n**bold** and a line that is very much longer than twenty columns";

        let mut plain = Vec::new();
        {
            let mut surface = PlainSurface::new(&mut plain);
            surface.fragment(source);
        }
        assert_eq!(String::from_utf8(plain).unwrap(), source);

        // Colour on, renderer still absent: the two answers are independent, and
        // only `with_markdown` turns the transform on.
        let mut colored = Vec::new();
        {
            let mut surface = PlainSurface::with_color(&mut colored, true);
            surface.fragment(source);
        }
        assert_eq!(String::from_utf8(colored).unwrap(), source);
    }
}
