//! Markdown layout — content only (REQ-592 BR-10).
//!
//! What this module decides is a **pure function of text and a width**: where a
//! line breaks, which inline runs are styled, what construct a line is. No
//! terminal, no clock, no I/O, nothing to mock. Turning those decisions into
//! bytes — the SGR, the defusing, the pending-line buffer — belongs to
//! `render.rs`, which is the `Surface` seam and the only thing in this feature
//! that touches a terminal.
//!
//! ## Why the split is a requirement rather than a preference
//!
//! Markdown rendering only happens when stdout is a terminal (BR-7), and
//! `cli_e2e` drives `teton` over pipes. A renderer written the obvious way —
//! layout decided at the point the bytes are written — would therefore ship with
//! **no automated coverage of its decisions at all**, and the TTY gate would be
//! the reason: the thing that hides the feature from piped users hides it from
//! the test suite too ([[LESSON-481]]).
//!
//! So the decisions are made here, where a unit test can read them with the gate
//! out of the way, and only the few bytes that reach the terminal stay gated.
//! This is `status.rs`'s shape deliberately — `status_line(level, effort,
//! width)` takes the width as a parameter rather than querying it, and a
//! structural sweep asserts the module never names `print!`/`stdout`. The same
//! sweep is at the bottom of this file, because BR-10 is otherwise a claim
//! nobody checks.
//!
//! ## The width is a parameter, always
//!
//! Nothing here reads the terminal. [`DEFAULT_WIDTH`] records the value the
//! query falls back to when there is no terminal, so a test can drive the
//! no-terminal case by name; the query itself lives in `prompt.rs` and is called
//! by the wiring, not by this module.
//!
//! ## Degrading, not truncating
//!
//! `status.rs` supplies the failure posture as well as the shape: a row that
//! does not fit is dropped whole, never clipped. The analogue here is that
//! **nothing is ever discarded**. A word wider than the whole terminal is
//! emitted on its own row, over-wide, rather than cut; a width too narrow even
//! for one word per row still emits every word. An over-wide row is ugly and the
//! terminal will hard-wrap it — a clipped row is a lie, and for a security
//! finding it is the kind of lie that loses the sentence that mattered.
//!
//! ## The recognized set is closed, and everything else is literal
//!
//! [`classify`] and [`parse_inline`] implement REQ-592's recognized-construct
//! table and nothing beyond it. This is OQ-2's hand-rolled decision, and its
//! recorded cost is that CommonMark constructs the table does not name — nested
//! lists, setext headings, indented code blocks, nested emphasis, a `|` inside a
//! code span inside a table cell — come back as **literal text**. Literal text
//! is legible-but-unstyled; it is not a parse error, it drops no characters, and
//! it never styles half of a construct. That fallback is what makes the
//! hand-rolled decision defensible, so it is asserted rather than assumed
//! (AC-14).
//!
//! Two boundary rules make the fallthrough fall out of the design instead of
//! being bolted onto it:
//!
//! - **Block constructs are recognized at column zero only.** An indented line is
//!   a paragraph carrying its indent, which makes a nested list item and an
//!   indented code block literal by the same rule rather than by two special
//!   cases. CommonMark's "up to three leading spaces" allowance is deliberately
//!   not implemented; models emit flush-left blocks.
//! - **Inline runs do not nest.** A run whose content contains the marker
//!   character is emitted whole, markers included, as literal text — so nested
//!   emphasis produces no styling at all rather than styling the outer half of
//!   it.

// Nothing outside this module calls into it yet: REQ-592 splits the layout
// decisions (here) from the bytes that carry them (`render.rs`), and the surface
// that consumes every function below arrives in TASK-279 of this same REQ. The
// allow is therefore about **task ordering**, not about code nobody wants — the
// house rule against a lingering `allow(dead_code)` (tetond's ADR-J: implied
// dead code is how a deletion ends up owned by nobody) is satisfied by naming
// the consumer and the condition. **Delete this attribute in TASK-279**, where
// `PlainSurface::with_markdown` starts calling these; if it is still here when
// the REQ closes, either the wiring never landed or a function here has no
// caller, and both are findings rather than warnings to keep suppressed.
#![allow(dead_code)]

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The width used when there is no terminal to ask.
///
/// `prompt.rs`'s width query already falls back to a conservative 80 columns,
/// and this constant is that same figure named so a test can drive the
/// no-terminal case without one (AC-3). The two are pinned to each other by
/// [`tests::the_no_terminal_default_matches_the_width_query_it_stands_in_for`],
/// because a default that silently drifted from the query's fallback would make
/// every test written against this constant a test of nothing.
pub const DEFAULT_WIDTH: usize = 80;

/// How many columns a tab is charged.
///
/// Tabs survive defusing — a diff line of indented source is ordinary content —
/// but a tab's real advance depends on the column it starts from, which a pure
/// width function does not know. Charging the full tab stop is the one choice
/// that can only ever **over**-estimate: a tab at column 0 advances exactly 8,
/// and from any other column it advances less. Over-estimating costs a row a few
/// columns of usable width; under-estimating puts the row past the terminal's
/// edge and hands it back to the hard wrap this feature exists to remove.
const TAB_COLUMNS: usize = 8;

/// The character a thematic break is drawn with — the same box-drawing rule
/// `cost_ui.rs` already uses for its summary banner, so the two do not disagree
/// about what a horizontal line looks like.
const RULE: char = '─';

/// The display width of `text` in terminal columns.
///
/// **Not `chars().count()`**, and the difference is the whole reason REQ-592
/// took a dependency (ADR-5). A CJK ideograph is one `char` and two columns; a
/// row measured by character count fits "within" the terminal and is then
/// hard-wrapped mid-word by the terminal itself, which is precisely the defect
/// this feature removes. For a property that disables the feature when it is
/// wrong, the maintained table wins.
///
/// The tab branch is why this is not simply the crate's own one-liner:
/// `unicode-width` charges a tab zero columns, correctly — it is a control
/// character, and its advance is a property of the cursor rather than of the
/// character. Since this function has no cursor, it charges [`TAB_COLUMNS`],
/// which is the conservative direction (see that constant).
///
/// What remains wrong under either choice: grapheme clusters. A ZWJ emoji
/// sequence measures as the sum of its parts rather than as one glyph, and
/// fixing that needs `unicode-segmentation` as well. Recorded in REQ-592's
/// Assumptions, out of scope here.
#[must_use]
pub fn display_width(text: &str) -> usize {
    if text.contains('\t') {
        text.chars().map(char_display_width).sum()
    } else {
        UnicodeWidthStr::width(text)
    }
}

/// The display width of a single character, with [`display_width`]'s tab rule.
#[must_use]
pub fn char_display_width(c: char) -> usize {
    if c == '\t' {
        TAB_COLUMNS
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

/// A horizontal rule `width` columns wide (the thematic-break rendering).
///
/// Degrades to the empty string at width zero rather than to a one-character
/// stub, since a rule shorter than its own construct is noise rather than
/// structure — the same "drop it whole" instinct `status.rs` applies to a row
/// that does not fit.
#[must_use]
pub fn thematic_break(width: usize) -> String {
    RULE.to_string().repeat(width)
}

// ---- Word wrap (BR-3) ------------------------------------------------------

/// The byte ranges of each whitespace-separated word in `text`.
fn word_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut open: Option<usize> = None;
    for (at, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = open.take() {
                spans.push(start..at);
            }
        } else if open.is_none() {
            open = Some(at);
        }
    }
    if let Some(start) = open {
        spans.push(start..text.len());
    }
    spans
}

/// Break `text` into rows, as byte ranges into `text`, fitting `first_avail`
/// columns on the first row and `cont_avail` on every row after it.
///
/// The two widths are the **content** widths — whatever prefix a caller intends
/// to print (a list marker, a quote marker, a hanging indent) has already been
/// subtracted. Ranges rather than strings because the styled path needs to map
/// inline spans onto rows, and a range keeps the correspondence to the original
/// offsets that a freshly allocated `String` would throw away.
///
/// A row's range runs from the first word's start to the last word's end, so
/// **whitespace inside a row is preserved exactly** — the two spaces someone
/// typed after a full stop survive. Whitespace at a break is consumed, which is
/// what a break is.
///
/// Three rules, each one an acceptance criterion:
///
/// - a break only ever lands on whitespace, never inside a word (AC-3);
/// - a word wider than the available columns is emitted **whole on its own row**
///   rather than split or truncated (AC-3), so the row is over-wide and complete
///   instead of tidy and lossy;
/// - an available width of zero is treated as one column, which by the previous
///   rule means one word per row. A caller that hands in a prefix wider than the
///   terminal still gets every word.
#[must_use]
pub fn wrap_ranges(text: &str, first_avail: usize, cont_avail: usize) -> Vec<Range<usize>> {
    let words = word_spans(text);
    let mut rows: Vec<Range<usize>> = Vec::new();
    let mut next = 0;
    while next < words.len() {
        let avail = if rows.is_empty() {
            first_avail
        } else {
            cont_avail
        }
        .max(1);
        let start = words[next].start;
        // The first word is taken unconditionally: that is the over-wide-but-
        // whole rule, and without it a long token would loop forever.
        let mut end = words[next].end;
        let mut last = next + 1;
        while last < words.len() {
            let candidate = words[last].end;
            if display_width(&text[start..candidate]) <= avail {
                end = candidate;
                last += 1;
            } else {
                break;
            }
        }
        rows.push(start..end);
        next = last;
    }
    rows
}

/// [`wrap_ranges`], rendered to strings with a prefix on each row.
///
/// `first_prefix` goes on the first emitted row and `cont_prefix` on the rest,
/// which is the hanging indent every block construct needs: `- ` then two
/// spaces for a list item, `> ` on every row for a block quote. Both prefixes
/// are measured in display columns and subtracted from `width`, so a wide marker
/// narrows the text rather than pushing the row past the edge.
#[must_use]
pub fn wrap_indented(
    text: &str,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
) -> Vec<String> {
    let first_avail = width.saturating_sub(display_width(first_prefix));
    let cont_avail = width.saturating_sub(display_width(cont_prefix));
    wrap_ranges(text, first_avail, cont_avail)
        .into_iter()
        .enumerate()
        .map(|(row, span)| {
            let prefix = if row == 0 { first_prefix } else { cont_prefix };
            let mut out = String::with_capacity(prefix.len() + span.len());
            out.push_str(prefix);
            out.push_str(&text[span]);
            out
        })
        .collect()
}

/// A paragraph wrapped at `width`, with no prefix.
///
/// Empty or all-whitespace text yields **no rows** rather than one empty row:
/// paragraph separation is [`Block::Blank`]'s job, and a wrap that invented a
/// row would double every blank line.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_indented(text, width, "", "")
}

/// The columns a list item's continuation rows are indented by, so that they
/// align under the item's *text* rather than under its marker (BR-3).
///
/// The marker's own width plus the single space that follows it — `- ` is two
/// columns, `10.` is four.
#[must_use]
pub fn hanging_indent(marker: &str) -> usize {
    display_width(marker) + 1
}

/// A list item wrapped at `width`, its marker printed once and its continuation
/// rows aligned under the text (AC-3).
#[must_use]
pub fn wrap_list_item(marker: &str, text: &str, width: usize) -> Vec<String> {
    let first = format!("{marker} ");
    let cont = " ".repeat(hanging_indent(marker));
    wrap_indented(text, width, &first, &cont)
}

/// A block quote wrapped at `width`, with the quote marker **preserved on every
/// emitted row** — a continuation row without it reads as unquoted prose, which
/// is a misattribution of who said it.
#[must_use]
pub fn wrap_block_quote(text: &str, width: usize) -> Vec<String> {
    wrap_indented(text, width, QUOTE_PREFIX, QUOTE_PREFIX)
}

/// What a quoted row carries.
const QUOTE_PREFIX: &str = "> ";

// ---- Inline spans (BR-5) ---------------------------------------------------

/// An inline style the renderer draws. Deliberately a closed set: `render.rs`
/// maps each one to fixed SGR parameters in the same shape as `LineKind::sgr()`,
/// so the alphabet the sanitizer destroys is owned by the seam and never by the
/// model's own bytes ([[LESSON-517]]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineStyle {
    /// `**text**`.
    Strong,
    /// `*text*` outside a code span.
    Emphasis,
    /// `` `text` ``.
    Code,
}

/// A styled run, as a byte range into [`Inline::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    /// How the run is drawn.
    pub style: InlineStyle,
    /// First byte of the run in [`Inline::text`].
    pub start: usize,
    /// One past the last byte of the run in [`Inline::text`].
    pub end: usize,
}

/// The result of parsing one line's inline markup: the text as it should be
/// **displayed**, plus where the styling goes.
///
/// The markers themselves are gone from `text` — that is the whole point, since
/// printing `**bold**` verbatim is the defect. Spans index `text`, not the
/// source, so wrapping and styling both measure the same string and cannot
/// disagree about where a row ends ([[LESSON-529]]: a display helper that
/// disagrees with the consuming parser is a lie on screen).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inline {
    /// The text to display, with recognized markers removed.
    pub text: String,
    /// The styled runs, in source order, never overlapping.
    pub spans: Vec<InlineSpan>,
}

impl Inline {
    /// The style covering byte `at` of [`Inline::text`], if any.
    #[must_use]
    pub fn style_at(&self, at: usize) -> Option<InlineStyle> {
        self.spans
            .iter()
            .find(|span| span.start <= at && at < span.end)
            .map(|span| span.style)
    }
}

/// The index of the next `*` at or after `from` that is **not** part of a `**`.
fn next_lone_star(source: &str, from: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'*' {
            if bytes.get(at + 1) == Some(&b'*') {
                at += 2;
                continue;
            }
            return Some(at);
        }
        at += 1;
    }
    None
}

/// Whether a run's content is one this module will style, or one that falls
/// through to literal text.
///
/// Three refusals, and each is a fallthrough rather than an error:
///
/// - **empty** — `****` and `**` style nothing, so styling them would remove
///   characters the reader is entitled to see;
/// - **contains the marker character** — this is the nested case (`**bold with
///   *italic* inside**`), which REQ-592 puts out of scope. Refusing the whole
///   run is what makes the fallthrough total: styling the outer half and leaving
///   the inner marker visible would be exactly the "partial styling" AC-14
///   forbids;
/// - **space-flanked** — `2 * 3 * 4` is arithmetic, not emphasis. CommonMark
///   reaches the same answer through its flanking rules; this is the cheap half
///   of that rule, and the half that matters for prose.
fn is_styleable(content: &str, marker: char) -> bool {
    !content.is_empty()
        && !content.contains(marker)
        && !content.starts_with([' ', '\t'])
        && !content.ends_with([' ', '\t'])
}

/// Parse one line's inline markup into display text plus styled runs.
///
/// Scanned left to right, one construct at a time, with code spans taking
/// priority at the position they open. That ordering is what makes `` `*.rs` ``
/// a code span containing a glob rather than a code span containing emphasis
/// (BR-6's rule, applied inline): a code span's content is **verbatim** and is
/// never re-scanned.
///
/// Anything unrecognized survives as itself. An unclosed marker is a literal
/// character; a nested run is literal from its opening marker to its closing
/// one. No input produces a panic and no input loses a character it did not
/// spend on a recognized marker — AC-14's unit half.
#[must_use]
pub fn parse_inline(source: &str) -> Inline {
    let bytes = source.as_bytes();
    let mut out = Inline::default();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'`' => {
                let Some(offset) = source[at + 1..].find('`') else {
                    out.text.push('`');
                    at += 1;
                    continue;
                };
                let close = at + 1 + offset;
                let content = &source[at + 1..close];
                if content.is_empty() {
                    out.text.push_str(&source[at..=close]);
                } else {
                    push_span(&mut out, content, InlineStyle::Code);
                }
                at = close + 1;
            }
            b'*' if source[at..].starts_with("**") => {
                let Some(offset) = source[at + 2..].find("**") else {
                    out.text.push_str("**");
                    at += 2;
                    continue;
                };
                let close = at + 2 + offset;
                let content = &source[at + 2..close];
                if is_styleable(content, '*') {
                    push_span(&mut out, content, InlineStyle::Strong);
                } else {
                    out.text.push_str(&source[at..close + 2]);
                }
                at = close + 2;
            }
            b'*' => {
                let Some(close) = next_lone_star(source, at + 1) else {
                    out.text.push('*');
                    at += 1;
                    continue;
                };
                let content = &source[at + 1..close];
                if is_styleable(content, '*') {
                    push_span(&mut out, content, InlineStyle::Emphasis);
                } else {
                    out.text.push_str(&source[at..=close]);
                }
                at = close + 1;
            }
            _ => {
                // Copy through to the next marker in one slice. `*` and a
                // backtick are both ASCII, so they can never fall inside a
                // multi-byte sequence and this stays UTF-8 safe.
                let next = source[at..]
                    .find(['*', '`'])
                    .map_or(source.len(), |offset| at + offset);
                out.text.push_str(&source[at..next]);
                at = next;
            }
        }
    }
    out
}

/// Append `content` to `inline`'s display text and record it as a styled run.
fn push_span(inline: &mut Inline, content: &str, style: InlineStyle) {
    let start = inline.text.len();
    inline.text.push_str(content);
    inline.spans.push(InlineSpan {
        style,
        start,
        end: inline.text.len(),
    });
}

// ---- Block classification --------------------------------------------------

/// One line's construct, from REQ-592's recognized-construct table.
///
/// Line-oriented on purpose: assistant text arrives token by token and must
/// render as it arrives, which is why OQ-2 rejected an event-based CommonMark
/// parser over a complete document. Every variant is decided from one line and
/// nothing else — with one exception the caller owns, below.
///
/// **Fence state belongs to the caller.** [`classify`] reports a [`Block::Fence`]
/// delimiter when it sees one but has no memory of whether a fence is currently
/// open, and inside an open fence a caller must not call this function at all:
/// BR-6 makes fence content verbatim, so classifying it would be the bug. The
/// state is one `bool` in the surface, which is where the buffering already
/// lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block<'a> {
    /// An empty or all-whitespace line. Preserved as paragraph separation,
    /// never collapsed.
    Blank,
    /// Three or more `-`, `*` or `_`. Drawn as a rule at the terminal width.
    ThematicBreak,
    /// `#`..`######` plus a space. The markers are not printed; the text is
    /// emphasized.
    Heading {
        /// How many `#` characters — 1 through 6.
        level: usize,
        /// The heading text, markers removed.
        text: &'a str,
    },
    /// A ` ``` ` fence delimiter, opening or closing. The marker is not printed.
    Fence {
        /// The info string after the backticks (a language, usually), or empty.
        info: &'a str,
    },
    /// A table row whose cells are only `-`, `:` and spaces — structure, drawn
    /// as a rule and never printed literally.
    TableSeparator {
        /// The separator's cells, so a caller can count columns.
        cells: Vec<&'a str>,
    },
    /// A line with a leading and trailing `|`, split into trimmed cells.
    TableRow {
        /// The row's cells, in order, each already trimmed.
        cells: Vec<&'a str>,
    },
    /// A leading `-`, `*`, `+` or `<digits>.` plus a space.
    ListItem {
        /// The marker as typed, without its trailing space.
        marker: &'a str,
        /// The item's text.
        text: &'a str,
    },
    /// A leading `>`.
    Quote {
        /// The quoted text, with the marker and one following space removed.
        text: &'a str,
    },
    /// Anything else — including every construct REQ-592 puts out of scope.
    Paragraph {
        /// The line's leading indentation in display columns, kept so an
        /// indented line renders with its indentation intact. Dropping it would
        /// be the "dropped characters" AC-14 forbids, and it is what makes an
        /// indented code block legible-but-unstyled rather than mangled.
        indent: usize,
        /// The text, with leading and trailing whitespace removed.
        text: &'a str,
    },
}

/// The cells of a table row, or `None` when `line` is not one.
///
/// A table row is a line with both a leading and a trailing `|`. The cells are
/// what lies between the pipes, trimmed — `| a | b |` is two cells, and `||` is
/// one empty cell.
///
/// Recognition only, and it is shared: the block classifier calls it to answer
/// for every row of the recognized-construct table, and [`measure_table`] calls
/// it again to split the buffered run it is handed. Column measurement and the
/// two layouts build on it in [`layout_table`], which is where a cell stops
/// being a slice of the source and becomes display text.
///
/// Escaped pipes (`\|`) are not honoured — a cell boundary is a `|`, full stop.
/// The related out-of-scope case, a `|` inside a code span inside a cell, is
/// caught by [`classify`] and sent to literal text rather than silently split.
#[must_use]
pub fn table_cells(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if trimmed.len() < 2 || !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    Some(inner.split('|').map(str::trim).collect())
}

/// Whether every cell is only `-`, `:` and spaces, with at least one `-` — the
/// alignment row under a table's header.
fn is_separator_row(cells: &[&str]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            !cell.is_empty()
                && cell.contains('-')
                && cell.chars().all(|c| matches!(c, '-' | ':' | ' '))
        })
}

/// Whether `line` is three or more of the same `-`, `*` or `_`, ignoring spaces.
fn is_thematic_break(line: &str) -> bool {
    let mut marker = None;
    let mut count = 0;
    for c in line.chars() {
        match c {
            ' ' | '\t' => {}
            '-' | '*' | '_' => {
                if *marker.get_or_insert(c) != c {
                    return false;
                }
                count += 1;
            }
            _ => return false,
        }
    }
    count >= 3
}

/// The `<digits>.` ordered-list marker at the start of `line`, if there is one.
fn ordered_marker(line: &str) -> Option<&str> {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || !line[digits..].starts_with(". ") {
        return None;
    }
    Some(&line[..=digits])
}

/// Classify one line of assistant text.
///
/// Never called for a line inside an open code fence — see [`Block`].
///
/// The order of the checks is load-bearing in one place: a thematic break is
/// tested **before** a list item, so `* * *` is a rule rather than a list item
/// whose text is `* *`. CommonMark resolves the same ambiguity the same way.
///
/// One ambiguity is resolved differently from CommonMark and is recorded rather
/// than hidden: a line of `---` under a paragraph is a setext heading to
/// CommonMark and a thematic break here. Setext headings are out of scope, and
/// the consequence is bounded — the heading's *text* is still literal prose,
/// which is what the fallthrough rule protects, and the underline draws a rule
/// instead of printing three dashes.
#[must_use]
pub fn classify(line: &str) -> Block<'_> {
    let body = line.trim_end();
    if body.trim_start().is_empty() {
        return Block::Blank;
    }

    // Column zero or nothing. An indented line is a paragraph that carries its
    // indentation, which is what makes a nested list item and an indented code
    // block literal text by the same rule (AC-14) instead of by two exceptions.
    let indent_bytes = body.len() - body.trim_start().len();
    if indent_bytes > 0 {
        return Block::Paragraph {
            indent: display_width(&body[..indent_bytes]),
            text: &body[indent_bytes..],
        };
    }

    if let Some(info) = body.strip_prefix("```") {
        // A fourth backtick is not a fence this module knows; it falls through
        // to literal text rather than being guessed at.
        if !info.contains('`') {
            return Block::Fence { info: info.trim() };
        }
    }

    if let Some(cells) = table_cells(body) {
        // A code span split across a cell boundary is out of scope, so the row
        // is literal text rather than a table row with a mangled cell. An odd
        // backtick count in any cell is exactly that case: the span opened in
        // one cell and closed in another.
        let span_crosses_a_cell = cells.iter().any(|cell| cell.matches('`').count() % 2 == 1);
        if !span_crosses_a_cell {
            return if is_separator_row(&cells) {
                Block::TableSeparator { cells }
            } else {
                Block::TableRow { cells }
            };
        }
        return Block::Paragraph {
            indent: 0,
            text: body,
        };
    }

    if is_thematic_break(body) {
        return Block::ThematicBreak;
    }

    let hashes = body.len() - body.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && body[hashes..].starts_with(' ') {
        return Block::Heading {
            level: hashes,
            text: body[hashes..].trim(),
        };
    }

    if let Some(rest) = body
        .strip_prefix("- ")
        .or_else(|| body.strip_prefix("* "))
        .or_else(|| body.strip_prefix("+ "))
    {
        return Block::ListItem {
            marker: &body[..1],
            text: rest.trim(),
        };
    }
    if let Some(marker) = ordered_marker(body) {
        return Block::ListItem {
            marker,
            text: body[marker.len() + 1..].trim(),
        };
    }

    if let Some(rest) = body.strip_prefix('>') {
        return Block::Quote {
            text: rest.strip_prefix(' ').unwrap_or(rest).trim_end(),
        };
    }

    Block::Paragraph {
        indent: 0,
        text: body,
    }
}

// ---- Table layout (BR-4) ---------------------------------------------------

/// What sits between two columns in the aligned layout.
///
/// Two spaces rather than a `│`: the columns are already separated by the fact
/// that they line up, and a vertical rule would have to be defused-safe, widened
/// for the CJK case, and argued about. The rule under the header carries the
/// "this is a table" signal on its own.
const COLUMN_GUTTER: &str = "  ";

/// What separates a column's name from its value in the transposed layout.
const LABEL_SEPARATOR: &str = ": ";

/// The columns a transposed value's continuation rows are indented by.
///
/// Deliberately **not** the label's own width. Aligning continuation rows under
/// the value would read better, but it makes the usable text width a function of
/// the widest column name — a table whose header is `Recommended remediation`
/// would lose 25 columns on every row of every block. A fixed two columns is
/// enough to show that a row is a continuation and costs the value nothing.
const TRANSPOSED_INDENT: usize = 2;

/// What a column with no name in the header row is labelled, plus its 1-based
/// position — `Column 3`.
///
/// A positional label rather than no label at all: a value printed bare in a
/// labelled block is a value the reader cannot attribute to a column, and the
/// two ways a column ends up unnamed (a header row with fewer cells than the
/// body, and a `|` inside a cell splitting one column into two) are exactly the
/// cases where attribution matters most.
const UNNAMED_COLUMN: &str = "Column";

/// A measured table: every row's cells as **display text**, plus each column's
/// width in terminal columns.
struct Table {
    /// One entry per source row, in source order. `None` where a separator row
    /// stood — it is structure, and the layouts draw it rather than print it.
    rows: Vec<Option<Vec<String>>>,
    /// Each column's display width, taken from the widest cell in it.
    columns: Vec<usize>,
}

impl Table {
    /// The terminal columns the aligned layout would need: every column at its
    /// measured width, plus one gutter between each adjacent pair.
    fn aligned_width(&self) -> usize {
        let gutters = self.columns.len().saturating_sub(1) * display_width(COLUMN_GUTTER);
        self.columns.iter().sum::<usize>() + gutters
    }
}

/// Measure a buffered run of table rows, or `None` when it is not one.
///
/// Two things happen here that the rest of the layout then depends on.
///
/// **Cells are reduced to display text.** Every cell goes through
/// [`parse_inline`], so a cell of `**bold**` measures four columns rather than
/// eight and a column lines up against what the reader sees rather than against
/// what the model typed. The stripped text is what the layouts then emit — see
/// [`layout_table`] for why that is a contract and not an implementation
/// detail.
///
/// **The table's shape comes from its content, not from its separator.** The
/// column count is the widest *data* row, so a row carrying a stray `|` widens
/// the table by a column rather than losing the text after the pipe, and a
/// separator row that declares more columns than anything else has does not
/// invent an empty one.
fn measure_table(rows: &[&str]) -> Option<Table> {
    let mut cells: Vec<Option<Vec<String>>> = Vec::with_capacity(rows.len());
    for row in rows {
        // The caller buffers a run of rows it classified; a line that is not a
        // table row means the run is not the shape this function was promised.
        // Guessing at it is the one move ADR-2 rules out, so the whole run
        // degrades to its own source.
        let raw = table_cells(row)?;
        if is_separator_row(&raw) {
            cells.push(None);
        } else {
            cells.push(Some(
                raw.iter().map(|cell| parse_inline(cell).text).collect(),
            ));
        }
    }

    let mut columns: Vec<usize> = Vec::new();
    for row in cells.iter().flatten() {
        for (at, cell) in row.iter().enumerate() {
            if columns.len() <= at {
                columns.push(0);
            }
            columns[at] = columns[at].max(display_width(cell));
        }
    }

    Some(Table {
        rows: cells,
        columns,
    })
}

/// Lay a buffered run of table rows out at `width` (BR-4).
///
/// The caller holds the run — consecutive table rows arrive one at a time and
/// the run ends when a non-table line does, which is buffering and belongs to
/// the surface. This function is the decision the buffer exists to make:
/// `(rows, width) -> Vec<String>`, with no memory between calls.
///
/// Three outcomes, tried in order:
///
/// 1. **The columns fit.** Cells are padded so each column lines up vertically
///    and each separator row is drawn as a rule. This is the layout that looks
///    like a table.
/// 2. **They do not.** Each data row becomes its own labelled block — one line
///    per column carrying that column's header and this row's value, the value
///    wrapped under BR-3, blocks separated by a blank line. This is the layout
///    that actually fixes the reported defect: the audit table that motivated
///    REQ-592 has a second column measuring 155..243 columns, and no terminal
///    is wide enough to align it.
/// 3. **Not even that fits.** The raw source rows are emitted unchanged, pipes
///    and all. They will be over-wide and the terminal will hard-wrap them —
///    which is ADR-2's posture, because the alternative is clipping cells, and a
///    clipped security finding is the sentence that mattered going missing.
///
/// ## The returned rows are final display text
///
/// Inline markers are already **removed** from what comes back: `**bold**` is
/// emitted as `bold`. A caller must not run [`parse_inline`] over these rows.
///
/// That is a contract rather than a convenience. The padding is computed from
/// the stripped width, so a second pass that stripped markers again would move
/// text left by four columns per marker pair and un-align every column the first
/// pass lined up — a display helper disagreeing with the consuming parser, which
/// is [[LESSON-529]] exactly. Keeping the markers in the output instead has the
/// same defect from the other end: `| **a | b** |` parses as two literal cells
/// but as one strong run once joined, so the width measured here and the width
/// drawn there would differ by four columns for reasons no test would predict.
///
/// The cost, recorded rather than hidden: **inline styling is not available
/// inside a table.** A bold cell renders as unstyled text at the right column
/// rather than as bold text at the wrong one.
///
/// ## What is still allowed past the edge
///
/// A single word wider than the available columns, per [`wrap_ranges`] — the
/// whole-and-over-wide rule. Nothing is truncated to buy the promise.
#[must_use]
pub fn layout_table(rows: &[&str], width: usize) -> Vec<String> {
    let Some(table) = measure_table(rows) else {
        return raw_rows(rows);
    };
    if table.aligned_width() <= width {
        return aligned_table(&table);
    }
    transposed_table(&table, width).unwrap_or_else(|| raw_rows(rows))
}

/// The source rows, unchanged — ADR-2's degrade-don't-truncate floor.
///
/// Separator rows included: this is the *source*, and a run that could not be
/// laid out has no structure left to draw, only text to preserve.
fn raw_rows(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| (*row).to_owned()).collect()
}

/// The aligned layout: cells padded to their column's width, separator rows
/// drawn as a rule spanning the table.
///
/// The rule spans the *table's* width rather than the terminal's — a rule out to
/// the right edge would claim columns the table does not occupy, and next to a
/// 20-column table on a 200-column terminal that reads as a page break rather
/// than as a header underline.
fn aligned_table(table: &Table) -> Vec<String> {
    let rule = thematic_break(table.aligned_width());
    table
        .rows
        .iter()
        .map(|row| {
            let Some(cells) = row else {
                return rule.clone();
            };
            let mut out = String::new();
            for (at, target) in table.columns.iter().enumerate() {
                if at > 0 {
                    out.push_str(COLUMN_GUTTER);
                }
                let cell = cells.get(at).map_or("", String::as_str);
                out.push_str(cell);
                out.push_str(&" ".repeat(target.saturating_sub(display_width(cell))));
            }
            // The last column's padding is invisible, and shipping it would put
            // trailing whitespace on every row of every table — which a terminal
            // shows only when the cursor lands in it and a `git diff` shows
            // always.
            out.trim_end().to_owned()
        })
        .collect()
}

/// The transposed layout: one labelled block per data row, or `None` when the
/// width cannot carry even that.
///
/// The header is the first non-separator row, and the data rows are the rest of
/// them. Separator rows vanish entirely here: they separate a header from a body
/// that this layout no longer prints in that arrangement, so there is nothing
/// left for a rule to divide.
///
/// Three refusals, each returning `None` so that [`layout_table`] falls back to
/// the raw source rather than emitting something worse:
///
/// - **the width cannot hold the widest label** — every block's first line would
///   start over-wide, so no layout has happened and the source at least keeps
///   its cell boundaries visible;
/// - **there are no data rows** — a header-only table has nothing to transpose,
///   and emitting nothing would discard the header;
/// - **every data row is empty** — same reason, from the other end.
fn transposed_table(table: &Table, width: usize) -> Option<Vec<String>> {
    let header = table.rows.iter().flatten().next()?;

    let prefixes: Vec<String> = (0..table.columns.len())
        .map(|at| {
            let name = header.get(at).map_or("", String::as_str).trim();
            if name.is_empty() {
                format!("{UNNAMED_COLUMN} {}{LABEL_SEPARATOR}", at + 1)
            } else {
                format!("{name}{LABEL_SEPARATOR}")
            }
        })
        .collect();

    // One column of value is the floor. Below it the label alone is already past
    // the terminal's edge, and a "layout" whose every first line overflows is
    // not a layout — it is the raw row with the pipes replaced by worse.
    let widest_label = prefixes.iter().map(|p| display_width(p)).max()?;
    if width <= widest_label {
        return None;
    }

    let continuation = " ".repeat(TRANSPOSED_INDENT);
    let mut out: Vec<String> = Vec::new();
    for row in table.rows.iter().flatten().skip(1) {
        let mut block: Vec<String> = Vec::new();
        for (at, prefix) in prefixes.iter().enumerate() {
            let value = row.get(at).map_or("", String::as_str);
            // An empty cell is a column this row has nothing to say about.
            // Printing `Finding: ` with nothing after it spends a line to say
            // so, and a row missing its trailing cells would spend several.
            if value.trim().is_empty() {
                continue;
            }
            block.extend(wrap_indented(value, width, prefix, &continuation));
        }
        if block.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(block);
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::scan;

    /// Every row a wrap emitted, measured in display columns.
    fn widths(rows: &[String]) -> Vec<usize> {
        rows.iter().map(|row| display_width(row)).collect()
    }

    // ---- Display width (ADR-5) --------------------------------------------

    /// The measurement REQ-592 took a dependency for: a CJK ideograph is one
    /// character and **two** columns, and a row measured in characters is a row
    /// the terminal hard-wraps.
    #[test]
    fn display_width_counts_columns_not_characters() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!("日本語".chars().count(), 3);
        assert_eq!(display_width("日本語"), 6);
        // Mixed content is the realistic case and the one a character count
        // gets wrong by a variable amount.
        assert_eq!(display_width("a日b"), 4);
    }

    /// A tab is charged a whole tab stop — the only direction that cannot put a
    /// row past the terminal's edge.
    #[test]
    fn a_tab_is_charged_a_full_tab_stop() {
        assert_eq!(display_width("\t"), TAB_COLUMNS);
        assert_eq!(display_width("ab\tcd"), 4 + TAB_COLUMNS);
        assert_eq!(char_display_width('\t'), TAB_COLUMNS);
        assert_eq!(char_display_width('語'), 2);
    }

    // ---- AC-3: the wrap ----------------------------------------------------

    /// A paragraph wider than the width breaks at spaces and never mid-word.
    #[test]
    fn a_paragraph_breaks_at_spaces_and_never_mid_word() {
        let text = "the quick brown fox jumps over the lazy dog and keeps on running";
        let rows = wrap(text, 20);
        assert!(rows.len() > 1, "this fixture must actually wrap: {rows:?}");
        for row in &rows {
            assert!(
                display_width(row) <= 20,
                "row {row:?} is {} columns wide at width 20",
                display_width(row)
            );
        }
        // Every word survives, whole and in order. A mid-word break would show
        // up here as a word the source never contained.
        let source_words: Vec<&str> = text.split_whitespace().collect();
        let emitted_words: Vec<&str> = rows.iter().flat_map(|row| row.split_whitespace()).collect();
        assert_eq!(source_words, emitted_words);
        // And no row is padded or left with a break's whitespace on it.
        for row in &rows {
            assert_eq!(row.trim(), row.as_str(), "row {row:?} carries edge space");
        }
    }

    /// A single token longer than the width occupies its own row **intact** —
    /// over-wide and complete rather than tidy and clipped (ADR-2).
    #[test]
    fn a_token_longer_than_the_width_gets_its_own_row_intact() {
        let long = "defense-in-depth-and-then-some-more-of-it";
        let rows = wrap(&format!("a {long} b"), 10);
        assert!(
            rows.contains(&long.to_owned()),
            "the long token must survive whole: {rows:?}"
        );
        assert_eq!(rows, vec!["a".to_owned(), long.to_owned(), "b".to_owned()]);
    }

    /// A list item's continuation rows align under its text, not under its
    /// marker.
    #[test]
    fn a_list_items_continuation_rows_align_under_its_text() {
        let rows = wrap_list_item("-", "alpha bravo charlie delta echo foxtrot golf", 20);
        assert!(rows.len() > 1, "this fixture must wrap: {rows:?}");
        assert!(rows[0].starts_with("- "), "{rows:?}");
        for row in &rows[1..] {
            assert!(
                row.starts_with("  ") && !row.starts_with("   "),
                "continuation row {row:?} must align under the item's text"
            );
        }
        for row in &rows {
            assert!(display_width(row) <= 20, "{row:?}");
        }

        // A two-digit ordered marker hangs four columns, not two.
        assert_eq!(hanging_indent("-"), 2);
        assert_eq!(hanging_indent("10."), 4);
        let ordered = wrap_list_item("10.", "alpha bravo charlie delta echo foxtrot", 20);
        assert!(ordered[0].starts_with("10. "), "{ordered:?}");
        for row in &ordered[1..] {
            assert!(row.starts_with("    "), "{row:?}");
        }
    }

    /// A block quote keeps its marker on **every** row — a continuation row
    /// without it reads as the session's own prose rather than as quoted text.
    #[test]
    fn a_block_quote_keeps_its_marker_on_every_row() {
        let rows = wrap_block_quote("alpha bravo charlie delta echo foxtrot golf hotel", 20);
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(row.starts_with("> "), "{row:?}");
            assert!(display_width(row) <= 20, "{row:?}");
        }
    }

    /// AC-3's no-terminal leg: the default width is 80, and wrapping at it
    /// produces rows that fit 80 columns. Runs under plain `cargo test` with no
    /// pty and no TTY, which is BR-10's claim.
    #[test]
    fn eighty_is_the_no_terminal_width() {
        assert_eq!(DEFAULT_WIDTH, 80);
        let text = "the quick brown fox ".repeat(20);
        let rows = wrap(&text, DEFAULT_WIDTH);
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(display_width(row) <= DEFAULT_WIDTH, "{row:?}");
        }
    }

    /// The other half of the same claim: this module's stand-in for the width
    /// query must not drift from the query's own fallback.
    ///
    /// Structural rather than behavioural because the query reads a real
    /// terminal, and a unit test has none — reading it here would make the
    /// assertion depend on how the suite was launched. Scanning the source
    /// instead keeps the two figures pinned to each other with no TTY involved.
    #[test]
    fn the_no_terminal_default_matches_the_width_query_it_stands_in_for() {
        let prompt = scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "prompt.rs")
            .map(|(_, src)| scan::code_only(&src))
            .expect("prompt.rs is a production source");
        let at = prompt
            .find("fn terminal_width()")
            .expect("prompt.rs owns the one place the width is queried");
        let body_end = at + prompt[at..].find("\n}").expect("a closing brace");
        let body = &prompt[at..body_end];
        assert!(
            body.contains(&DEFAULT_WIDTH.to_string()),
            "markdown::DEFAULT_WIDTH is {DEFAULT_WIDTH}, but the width query falls back to \
             something else:\n{body}\nEvery test written against the constant would then be \
             testing a width the CLI never uses off a terminal (AC-3)."
        );
    }

    /// **The measurement pin.** Wrapping uses `UnicodeWidthStr` /
    /// `UnicodeWidthChar`, not `chars().count()` — asserted with content where
    /// the two disagree, so the test fails if the measure is ever swapped back.
    ///
    /// Four three-ideograph words, six columns each, at width 13. Display width
    /// fits two words per row (6 + 1 + 6 = 13) and refuses a third (20). A
    /// character count would see 7 and 11 and pack **three** words onto the
    /// first row — a 20-column row on a 13-column terminal, which the terminal
    /// then hard-wraps mid-word. That is the defect REQ-592 exists to fix, one
    /// layer down.
    #[test]
    fn wrapping_measures_display_columns_not_characters() {
        let text = "日本語 日本語 日本語 日本語";
        let rows = wrap(text, 13);
        assert_eq!(
            rows,
            vec!["日本語 日本語".to_owned(), "日本語 日本語".to_owned()],
            "a character-count wrap would have fitted three words on the first row"
        );
        for row in &rows {
            assert_eq!(display_width(row), 13);
            assert_eq!(
                row.chars().count(),
                7,
                "this fixture is only meaningful while the two measures disagree"
            );
        }
    }

    /// **AC-3's CJK sweep: no emitted row exceeds the width in display
    /// columns.** Across a range of widths, on mixed CJK and Latin content whose
    /// widest token still fits the narrowest width tested — so the assertion is
    /// unconditional and an over-wide row is always a failure rather than the
    /// long-token rule.
    #[test]
    fn no_emitted_row_exceeds_the_width_in_display_columns() {
        let text = "設定 is 適用 to every 経路 名前 with 予算 and 制限 見積 for 監査 log";
        let widest_token = text
            .split_whitespace()
            .map(display_width)
            .max()
            .expect("the fixture has words");
        assert!(widest_token <= 8, "fixture token too wide: {widest_token}");

        for width in 8..=40 {
            let rows = wrap(text, width);
            assert!(!rows.is_empty(), "width {width} emitted nothing");
            for row in &rows {
                assert!(
                    display_width(row) <= width,
                    "width {width}: row {row:?} measures {} display columns (it measures {} \
                     characters, which is how a `chars().count()` wrap lets it through)",
                    display_width(row),
                    row.chars().count()
                );
            }
            // Nothing was dropped on the way.
            let emitted: Vec<&str> = rows.iter().flat_map(|row| row.split_whitespace()).collect();
            assert_eq!(text.split_whitespace().collect::<Vec<_>>(), emitted);
        }
    }

    /// The same sweep for the hanging-indent path, where the prefix eats into
    /// the width and an unsubtracted prefix would show as an over-wide row.
    ///
    /// Narrow widths are swept too, and there the one permitted over-wide row is
    /// a row whose **content** is a single word — the long-token rule (a word
    /// wider than the columns left over after the marker is emitted whole rather
    /// than cut). Asserting that distinction rather than skipping the narrow
    /// half is the point: an unsubtracted prefix would show up as a *multi-word*
    /// row over the width, which is exactly what this refuses.
    #[test]
    fn a_prefixed_row_stays_inside_the_width_too() {
        let text = "監査 findings 予算 and 制限 across 経路 名前 設定 適用 見積";
        for width in 4..=40 {
            for (first, cont) in [("- ", "  "), ("10. ", "    "), (QUOTE_PREFIX, QUOTE_PREFIX)] {
                let rows = wrap_indented(text, width, first, cont);
                for (n, row) in rows.iter().enumerate() {
                    let prefix = if n == 0 { first } else { cont };
                    assert!(row.starts_with(prefix), "width {width}: {row:?}");
                    if display_width(row) <= width {
                        continue;
                    }
                    assert_eq!(
                        row[prefix.len()..].split_whitespace().count(),
                        1,
                        "width {width}: {row:?} is {} columns and carries more than one word, \
                         so the prefix was not subtracted from the available width",
                        display_width(row)
                    );
                }
            }
        }
        // The convenience wrappers compose the same prefixes the sweep used.
        assert_eq!(
            wrap_list_item("-", text, 40),
            wrap_indented(text, 40, "- ", "  ")
        );
        assert_eq!(
            wrap_list_item("10.", text, 40),
            wrap_indented(text, 40, "10. ", "    ")
        );
        assert_eq!(
            wrap_block_quote(text, 40),
            wrap_indented(text, 40, QUOTE_PREFIX, QUOTE_PREFIX)
        );
    }

    /// Degrading, not truncating: a width too small for any sensible layout
    /// still emits every word, whole.
    #[test]
    fn an_impossible_width_still_emits_every_word() {
        let text = "alpha bravo charlie";
        for width in 0..4 {
            let rows = wrap(text, width);
            assert_eq!(
                rows,
                vec!["alpha".to_owned(), "bravo".to_owned(), "charlie".to_owned()],
                "width {width} must degrade to one word per row, not clip"
            );
        }
        // Even when the marker alone is wider than the terminal.
        let rows = wrap_list_item("10.", text, 2);
        assert_eq!(rows[0], "10. alpha");
        assert_eq!(&rows[1..], ["    bravo", "    charlie"]);
    }

    /// Whitespace inside a row is preserved; only whitespace at a break is
    /// consumed.
    #[test]
    fn interior_whitespace_survives_a_wrap() {
        let rows = wrap("one.  two three", 40);
        assert_eq!(rows, vec!["one.  two three".to_owned()]);
        assert!(wrap("", 40).is_empty(), "empty text emits no row");
        assert!(wrap("   ", 40).is_empty(), "blank text emits no row");
    }

    /// A rule spans the width it is given, and degrades to nothing rather than
    /// to a stub.
    #[test]
    fn a_thematic_break_is_a_rule_at_the_width() {
        assert_eq!(display_width(&thematic_break(10)), 10);
        assert_eq!(thematic_break(3), "───");
        assert_eq!(thematic_break(0), "");
    }

    // ---- The recognized-construct table ------------------------------------

    /// **Every row of REQ-592's recognized-construct table classifies as itself.**
    ///
    /// One case per row of that table, so a construct dropped from the
    /// classifier fails here by name rather than by a downstream rendering
    /// surprise. The three inline rows (strong, emphasis, code span) are
    /// [`parse_inline`]'s and are pinned below.
    #[test]
    fn every_recognized_block_construct_classifies_as_itself() {
        assert_eq!(classify(""), Block::Blank);
        assert_eq!(classify("   "), Block::Blank);

        assert_eq!(
            classify("just some prose"),
            Block::Paragraph {
                indent: 0,
                text: "just some prose"
            }
        );

        for level in 1..=6 {
            let line = format!("{} Heading", "#".repeat(level));
            assert_eq!(
                classify(&line),
                Block::Heading {
                    level,
                    text: "Heading"
                },
                "{line:?}"
            );
        }

        assert_eq!(
            classify("| a | b |"),
            Block::TableRow {
                cells: vec!["a", "b"]
            }
        );
        assert_eq!(
            classify("|---|:--:|"),
            Block::TableSeparator {
                cells: vec!["---", ":--:"]
            }
        );

        for marker in ["-", "*", "+"] {
            assert_eq!(
                classify(&format!("{marker} item")),
                Block::ListItem {
                    marker,
                    text: "item"
                },
                "{marker}"
            );
        }
        assert_eq!(
            classify("1. item"),
            Block::ListItem {
                marker: "1.",
                text: "item"
            }
        );
        assert_eq!(
            classify("12. item"),
            Block::ListItem {
                marker: "12.",
                text: "item"
            }
        );

        assert_eq!(classify("> quoted"), Block::Quote { text: "quoted" });
        assert_eq!(classify(">quoted"), Block::Quote { text: "quoted" });

        assert_eq!(classify("```"), Block::Fence { info: "" });
        assert_eq!(classify("```rust"), Block::Fence { info: "rust" });

        for rule in ["---", "***", "___", "- - -", "-----"] {
            assert_eq!(classify(rule), Block::ThematicBreak, "{rule}");
        }
    }

    /// A thematic break wins over a list item, so `* * *` is a rule rather than
    /// an item whose text is `* *`.
    #[test]
    fn a_rule_of_stars_is_not_a_list_item() {
        assert_eq!(classify("* * *"), Block::ThematicBreak);
        assert_eq!(
            classify("* item"),
            Block::ListItem {
                marker: "*",
                text: "item"
            }
        );
        // Two dashes is not a rule, and not a list item either.
        assert_eq!(
            classify("--"),
            Block::Paragraph {
                indent: 0,
                text: "--"
            }
        );
    }

    /// A near-miss on each construct falls through to literal text rather than
    /// being guessed at.
    #[test]
    fn near_misses_fall_through_to_literal_text() {
        for line in [
            "#NoSpace",             // heading needs a space
            "####### seven hashes", // beyond h6
            "-no space",            // list marker needs a space
            "1.no space",
            "| unterminated",
            "``` `backtick in the info string",
        ] {
            assert_eq!(
                classify(line),
                Block::Paragraph {
                    indent: 0,
                    text: line
                },
                "{line:?} should be literal text"
            );
        }
    }

    // ---- AC-14: the out-of-scope set is literal text ------------------------

    /// A nested list item is literal text, indentation and marker intact.
    #[test]
    fn a_nested_list_item_is_literal_text() {
        assert_eq!(
            classify("  - nested item"),
            Block::Paragraph {
                indent: 2,
                text: "- nested item"
            }
        );
        assert_eq!(
            classify("    * deeper"),
            Block::Paragraph {
                indent: 4,
                text: "* deeper"
            }
        );
        // The top-level item beside it still classifies, so this is a
        // fallthrough for the nested case and not a broken list rule.
        assert_eq!(
            classify("- top level"),
            Block::ListItem {
                marker: "-",
                text: "top level"
            }
        );
    }

    /// A setext heading is literal text: the title is prose, not a heading.
    #[test]
    fn a_setext_heading_is_literal_text() {
        assert_eq!(
            classify("Title"),
            Block::Paragraph {
                indent: 0,
                text: "Title"
            }
        );
        assert_eq!(
            classify("====="),
            Block::Paragraph {
                indent: 0,
                text: "====="
            }
        );
        // The `-----` underline form is the recorded divergence (see
        // `classify`): the title stays literal prose — which is what the
        // fallthrough rule protects — and the underline draws a rule.
        assert_eq!(classify("-----"), Block::ThematicBreak);
    }

    /// An indented code block is literal text that keeps its indentation, so
    /// the code is legible-but-unstyled rather than reflowed into prose.
    #[test]
    fn an_indented_code_block_is_literal_text() {
        assert_eq!(
            classify("    let x = *p;"),
            Block::Paragraph {
                indent: 4,
                text: "let x = *p;"
            }
        );
        let Block::Paragraph { indent, text } = classify("    let x = *p;") else {
            unreachable!()
        };
        let rows = wrap_indented(text, 40, &" ".repeat(indent), &" ".repeat(indent));
        assert_eq!(rows, vec!["    let x = *p;".to_owned()]);
    }

    /// Nested emphasis is literal text — **all** of it, markers included. The
    /// failure this guards against is partial styling: the outer run drawn bold
    /// with a stray `*` left visible inside it.
    #[test]
    fn nested_emphasis_is_literal_text() {
        for source in [
            "**bold with *italic* inside**",
            "*emphasis with **strong** inside*",
        ] {
            let inline = parse_inline(source);
            assert_eq!(inline.text, source, "{source:?} must survive verbatim");
            assert!(
                inline.spans.is_empty(),
                "{source:?} produced partial styling: {:?}",
                inline.spans
            );
        }
    }

    /// A `|` inside a code span inside a table cell is literal text: the whole
    /// row falls through rather than becoming a table row with a code span torn
    /// in half.
    #[test]
    fn a_pipe_inside_a_code_span_inside_a_table_cell_is_literal_text() {
        let row = "| flag | `--a|--b` | on |";
        assert_eq!(
            classify(row),
            Block::Paragraph {
                indent: 0,
                text: row
            },
            "a torn code span makes the row literal, not a mangled table row"
        );
        // Nothing is dropped: every character of the source is still there.
        let Block::Paragraph { text, .. } = classify(row) else {
            unreachable!()
        };
        assert_eq!(text, row);
        // A cell with a *complete* code span is still a table row, so the rule
        // above is about the torn case and not about code spans in tables.
        assert_eq!(
            classify("| flag | `--a` | on |"),
            Block::TableRow {
                cells: vec!["flag", "`--a`", "on"]
            }
        );
    }

    /// The whole out-of-scope set in one place: no panic, and no dropped
    /// characters, for every construct REQ-592 names.
    #[test]
    fn no_out_of_scope_construct_drops_a_character() {
        for line in [
            "  - nested item",
            "Title",
            "=====",
            "    indented code",
            "**bold with *italic* inside**",
            "| a | `x|y` | b |",
            "<div>inline html</div>",
            "- [ ] a task list item",
            "[ref]: https://example.invalid",
            "text with a footnote[^1]",
        ] {
            let block = classify(line);
            let rendered = match &block {
                Block::Paragraph { indent, text } => {
                    format!("{}{text}", " ".repeat(*indent))
                }
                Block::ListItem { marker, text } => format!("{marker} {text}"),
                other => panic!("{line:?} classified as {other:?}, which drops its source form"),
            };
            assert_eq!(rendered, line, "{line:?} lost characters");
            // And the inline pass keeps every character it does not spend on a
            // marker it recognized.
            let inline = parse_inline(line);
            assert!(
                !inline.text.is_empty(),
                "{line:?} lost its text to the inline pass"
            );
        }
    }

    // ---- Inline spans -------------------------------------------------------

    /// The three inline rows of the recognized-construct table: markers
    /// removed, the run recorded, the rest untouched.
    #[test]
    fn strong_emphasis_and_code_spans_are_recognized_with_markers_removed() {
        let inline = parse_inline("a **bold** and *soft* and `code` end");
        assert_eq!(inline.text, "a bold and soft and code end");
        let styles: Vec<InlineStyle> = inline.spans.iter().map(|span| span.style).collect();
        assert_eq!(
            styles,
            vec![
                InlineStyle::Strong,
                InlineStyle::Emphasis,
                InlineStyle::Code
            ]
        );
        for span in &inline.spans {
            let run = &inline.text[span.start..span.end];
            assert!(
                matches!(run, "bold" | "soft" | "code"),
                "span covers {run:?}"
            );
        }
        assert_eq!(inline.style_at(2), Some(InlineStyle::Strong));
        assert_eq!(inline.style_at(0), None);
    }

    /// A code span's content is verbatim: a `*` inside it is a shell glob, not
    /// emphasis (BR-6's rule applied inline).
    #[test]
    fn a_star_inside_a_code_span_is_not_emphasis() {
        let inline = parse_inline("run `rm *.rs` now");
        assert_eq!(inline.text, "run rm *.rs now");
        assert_eq!(inline.spans.len(), 1);
        assert_eq!(inline.spans[0].style, InlineStyle::Code);
        assert_eq!(
            &inline.text[inline.spans[0].start..inline.spans[0].end],
            "rm *.rs"
        );
    }

    /// An unclosed or empty marker is a literal character, not a swallowed one.
    #[test]
    fn an_unclosed_or_empty_marker_is_literal() {
        for source in [
            "an * orphan",
            "an ** orphan",
            "a ` orphan",
            "empty ** here",
            "empty `` here",
            "2 * 3 * 4 is arithmetic",
            "a *  spaced  * run",
        ] {
            let inline = parse_inline(source);
            assert_eq!(inline.text, source, "{source:?} must survive verbatim");
            assert!(inline.spans.is_empty(), "{source:?}: {:?}", inline.spans);
        }
    }

    /// The inline pass is total: no input panics, and the display text is never
    /// longer than its source (markers only ever come out).
    #[test]
    fn the_inline_pass_is_total() {
        for source in [
            "", "*", "**", "***", "****", "`", "``", "```", "*`*`*", "**`**`**", "a*b*c", "*日本*",
            "**日**", "`日`", "\t*x*\t",
        ] {
            let inline = parse_inline(source);
            assert!(
                inline.text.len() <= source.len(),
                "{source:?} grew to {:?}",
                inline.text
            );
            for span in &inline.spans {
                assert!(span.start < span.end, "{source:?}: empty span");
                assert!(
                    span.end <= inline.text.len(),
                    "{source:?}: span past the end"
                );
                // A span must land on a character boundary, or slicing it panics
                // in the renderer rather than here.
                assert!(inline.text.is_char_boundary(span.start), "{source:?}");
                assert!(inline.text.is_char_boundary(span.end), "{source:?}");
            }
        }
    }

    /// Spans never overlap, so the renderer can walk them in order and does not
    /// have to decide which of two styles wins.
    #[test]
    fn spans_are_ordered_and_disjoint() {
        let inline = parse_inline("**a** *b* `c` **d**");
        assert_eq!(inline.spans.len(), 4);
        let mut previous_end = 0;
        for span in &inline.spans {
            assert!(span.start >= previous_end, "{:?}", inline.spans);
            previous_end = span.end;
        }
    }

    // ---- Table recognition ---------------------------------------------------

    /// Cell splitting, which the classifier and TASK-278's layout share.
    #[test]
    fn table_cells_splits_on_pipes_and_trims() {
        assert_eq!(table_cells("| a | b |"), Some(vec!["a", "b"]));
        assert_eq!(table_cells("|a|"), Some(vec!["a"]));
        assert_eq!(table_cells("||"), Some(vec![""]));
        assert_eq!(
            table_cells("|  spaced  |  cells  |"),
            Some(vec!["spaced", "cells"])
        );
        assert_eq!(table_cells("no pipes"), None);
        assert_eq!(table_cells("| unterminated"), None);
        assert_eq!(table_cells("|"), None);
    }

    /// A separator row is structure; a row of dashes that is *not* only dashes
    /// is data.
    #[test]
    fn a_separator_row_is_only_dashes_colons_and_spaces() {
        assert!(matches!(
            classify("| --- | :--- |"),
            Block::TableSeparator { .. }
        ));
        assert!(matches!(classify("| ---x | --- |"), Block::TableRow { .. }));
        // Colons alone are not an alignment row.
        assert!(matches!(classify("| : | : |"), Block::TableRow { .. }));
    }

    // ---- AC-4: the two table layouts (BR-4) ---------------------------------

    /// The raw source rows, which is what every degrade path is compared to.
    fn source_rows(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_owned()).collect()
    }

    /// AC-4's fixture, read from disk.
    ///
    /// Resolved from `CARGO_MANIFEST_DIR` rather than from the current
    /// directory: `cargo test` run from the workspace root and from
    /// `crates/teton` have different working directories, and a relative path
    /// would pass under one and fail under the other for reasons that have
    /// nothing to do with the layout.
    fn audit_fixture() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../.adlc/specs/REQ-592-markdown-aware-terminal-rendering/fixtures/audit-2026-08-26.md",
        );
        std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "AC-4's fixture must be readable at {}: {err}",
                path.display()
            )
        })
    }

    /// AC-4, aligned half: a table whose columns fit is padded so the columns
    /// line up, and its separator row is **drawn** as a rule rather than
    /// printed.
    #[test]
    fn a_table_that_fits_lines_its_columns_up_and_rules_its_separator() {
        let rows = [
            "| Name | Status |",
            "|------|--------|",
            "| alpha | ok |",
            "| bravo-long | failed |",
        ];
        let out = layout_table(&rows, 40);

        assert_eq!(
            out,
            vec![
                "Name        Status".to_owned(),
                RULE.to_string().repeat(18),
                "alpha       ok".to_owned(),
                "bravo-long  failed".to_owned(),
            ]
        );

        // The alignment as a property rather than as a transcription: the second
        // column starts at the same display offset on every row, which is what
        // "lined up vertically" means when the rows have different widths.
        for (row, value) in [(&out[0], "Status"), (&out[2], "ok"), (&out[3], "failed")] {
            let at = row.find(value).expect("the second column's value");
            assert_eq!(display_width(&row[..at]), 12, "{row:?}");
        }

        // Nothing structural leaked through as text.
        for row in &out {
            assert!(!row.contains('|'), "{row:?} still carries a table pipe");
            assert!(
                !row.contains("---"),
                "the separator row was printed: {row:?}"
            );
            assert_eq!(
                row.trim_end(),
                row.as_str(),
                "{row:?} carries trailing space"
            );
        }

        // The rule spans the table, not the terminal — a 40-column rule next to
        // an 18-column table reads as a page break, not as a header underline.
        assert_eq!(display_width(&out[1]), 18);
    }

    /// AC-4, transposed half — the case the reported defect actually lives in.
    ///
    /// The table is **read from disk**, never transcribed. A table authored
    /// while knowing the layout algorithm tests the author's assumptions rather
    /// than the algorithm ([[LESSON-529]]'s re-enactment corollary), and this
    /// one is real model output that the renderer was not designed against:
    /// bold runs, code spans, arrows and em-dashes in a column that no terminal
    /// is wide enough to align.
    #[test]
    fn the_audit_fixture_transposes_at_a_hundred_and_at_two_hundred_columns() {
        let source = audit_fixture();
        let rows: Vec<&str> = source
            .lines()
            .filter(|line| table_cells(line).is_some())
            .collect();

        // The fixture's shape, **re-measured** rather than trusted — its own
        // header says to re-measure if the file changes, and a test that
        // asserted the header's figures would be asserting the comment.
        assert_eq!(
            rows.len(),
            9,
            "the fixture's table is a header, a separator and 7 data rows"
        );
        let values: Vec<String> = rows[2..]
            .iter()
            .map(|row| parse_inline(table_cells(row).expect("a table row")[1]).text)
            .collect();
        assert_eq!(values.len(), 7);
        let widest = values
            .iter()
            .map(|value| display_width(value))
            .max()
            .expect("seven values");
        assert!(
            widest > 200,
            "the second column has to overflow both widths or the test is vacuous: {widest}"
        );

        for width in [100_usize, 200] {
            let out = layout_table(&rows, width);

            for row in &out {
                assert!(
                    display_width(row) <= width,
                    "at {width} columns the row {row:?} is {} columns wide",
                    display_width(row)
                );
                assert!(
                    !row.contains('|'),
                    "at {width}: {row:?} is still raw source"
                );
                assert!(
                    !row.contains("---"),
                    "at {width}: the separator row was printed: {row:?}"
                );
            }

            // One labelled block per data row, blocks separated by a blank line.
            let blocks: Vec<&[String]> = out.split(String::is_empty).collect();
            assert_eq!(blocks.len(), 7, "at {width} columns: {out:#?}");

            for (block, value) in blocks.iter().zip(&values) {
                assert!(
                    block[0].starts_with("Surface: "),
                    "at {width}: {block:#?} does not lead with its first column"
                );
                let finding = block
                    .iter()
                    .position(|row| row.starts_with("Finding: "))
                    .unwrap_or_else(|| panic!("at {width}: no Finding line in {block:#?}"));

                // Every word of the value survives, in order: wrapped, never
                // clipped. A truncated cell would show up here as a missing
                // tail, which is precisely the failure ADR-2 forbids.
                let emitted: Vec<&str> = block[finding..]
                    .iter()
                    .flat_map(|row| row.split_whitespace())
                    .skip(1)
                    .collect();
                let expected: Vec<&str> = value.split_whitespace().collect();
                assert_eq!(emitted, expected, "at {width} columns");

                // Wrapping is asserted where it must happen rather than
                // everywhere. The label leaves `width - 9` columns; a value
                // wider than that has to occupy continuation rows, and one that
                // is not must not, because a break inserted into a value that
                // already fit would be a bug rather than a feature.
                let available = width - display_width("Finding: ");
                assert_eq!(
                    block.len() - finding > 1,
                    display_width(value) > available,
                    "at {width} columns: {block:#?}"
                );
            }

            // Non-vacuity for the wrap itself, stated per width because the two
            // widths exercise different halves of it: at 100 columns all seven
            // values (153..235 columns as measured above) overflow, at 200 only
            // the widest three do.
            let wrapped = blocks.iter().filter(|block| block.len() > 2).count();
            assert!(wrapped > 0, "at {width} columns nothing wrapped");
            if width == 100 {
                assert_eq!(wrapped, 7, "at 100 columns every value must wrap");
            }
        }
    }

    /// AC-4's measurement rule: a column is measured against what is
    /// **displayed**, not against what was typed.
    #[test]
    fn column_measurement_ignores_inline_markers() {
        assert_eq!(display_width("**bold**"), 8);
        assert_eq!(display_width(&parse_inline("**bold**").text), 4);

        let rows = ["| Key | Value |", "|-----|-------|", "| **bold** | x |"];
        let out = layout_table(&rows, 40);

        // The first column is four columns wide, not eight, so `Value` lands at
        // display column six. Measured against the source it would land at ten
        // and the header would sit four columns adrift of the body it labels.
        assert_eq!(
            out,
            vec![
                "Key   Value".to_owned(),
                RULE.to_string().repeat(11),
                "bold  x".to_owned(),
            ]
        );
    }

    /// ADR-2's floor: at a width too small to lay the table out even
    /// transposed, the **source** comes back — over-wide and complete rather
    /// than tidy and lossy.
    #[test]
    fn a_width_too_narrow_for_even_the_transposed_layout_emits_the_raw_rows() {
        let rows = [
            "| Finding |",
            "|---------|",
            "| a very long finding that does not fit |",
        ];

        // `Finding: ` is nine columns, so at eight there is not one column left
        // for a value: every block would open past the terminal's edge, which
        // is not a layout. The source at least keeps its cell boundaries.
        assert_eq!(layout_table(&rows, 8), source_rows(&rows));
        assert_eq!(layout_table(&rows, 9), source_rows(&rows));

        // One more column and the layout is possible again, if barely: one word
        // per row, every word intact.
        let barely = layout_table(&rows, 10);
        assert!(
            !barely.iter().any(|row| row.contains('|')),
            "width 10 must lay out rather than degrade: {barely:?}"
        );
        let words: Vec<&str> = barely
            .iter()
            .flat_map(|row| row.split_whitespace())
            .collect();
        assert_eq!(
            words,
            vec!["Finding:", "a", "very", "long", "finding", "that", "does", "not", "fit"]
        );
    }

    /// A `|` inside a cell splits it, because a cell boundary is a `|` full
    /// stop (see [`table_cells`]). The tail therefore becomes an extra column
    /// with no header — and it is **labelled by position rather than dropped**,
    /// which is the difference between an odd-looking table and a lost
    /// sentence.
    #[test]
    fn a_pipe_inside_a_cell_becomes_a_column_rather_than_a_lost_tail() {
        let rows = [
            "| Surface | Finding |",
            "|---------|---------|",
            "| shell | uses a | b pipeline |",
        ];
        assert_eq!(
            layout_table(&rows, 20),
            vec![
                "Surface: shell".to_owned(),
                "Finding: uses a".to_owned(),
                "Column 3: b pipeline".to_owned(),
            ]
        );
    }

    /// A row with fewer cells than the table has columns says nothing about the
    /// columns it is missing, so the block says nothing about them either —
    /// rather than spending a line on a bare `Finding: `.
    #[test]
    fn a_row_missing_its_trailing_cell_omits_that_columns_line() {
        let rows = [
            "| Surface | Finding |",
            "|---------|---------|",
            "| alpha | a finding that is much too wide to align |",
            "| bravo |",
        ];
        let out = layout_table(&rows, 20);

        assert_eq!(
            out,
            vec![
                "Surface: alpha".to_owned(),
                "Finding: a finding".to_owned(),
                "  that is much too".to_owned(),
                "  wide to align".to_owned(),
                String::new(),
                "Surface: bravo".to_owned(),
            ]
        );
        for row in &out {
            assert_ne!(row.trim_end(), "Finding:", "an empty label was printed");
        }
    }

    /// A header row shorter than the table's column count leaves a column
    /// unnamed. The value still needs attributing to something, so it is
    /// labelled by position.
    #[test]
    fn a_header_row_shorter_than_the_table_labels_the_rest_by_position() {
        let rows = [
            "| Surface |",
            "|---------|",
            "| alpha | an unheaded value that is far too wide to align |",
        ];
        let out = layout_table(&rows, 24);

        assert_eq!(
            out,
            vec![
                "Surface: alpha".to_owned(),
                "Column 2: an unheaded".to_owned(),
                "  value that is far too".to_owned(),
                "  wide to align".to_owned(),
            ]
        );
        assert!(widths(&out).iter().all(|w| *w <= 24), "{out:?}");
    }

    /// A single-column table has no columns to line up against each other, and
    /// it still has to obey both halves of BR-4.
    #[test]
    fn a_single_column_table_aligns_or_transposes_like_any_other() {
        let rows = [
            "| Finding |",
            "|---------|",
            "| the first finding, wider than the width |",
            "| the second finding |",
        ];

        // Wide enough: the values under a rule, with nothing else to align to.
        let aligned = layout_table(&rows, 60);
        assert_eq!(
            aligned,
            vec![
                "Finding".to_owned(),
                RULE.to_string().repeat(39),
                "the first finding, wider than the width".to_owned(),
                "the second finding".to_owned(),
            ]
        );

        // Too narrow: one labelled block per row, blocks separated by a blank
        // line, every value wrapped.
        let transposed = layout_table(&rows, 24);
        assert_eq!(
            transposed,
            vec![
                "Finding: the first".to_owned(),
                "  finding, wider than".to_owned(),
                "  the width".to_owned(),
                String::new(),
                "Finding: the second".to_owned(),
                "  finding".to_owned(),
            ]
        );
        assert!(
            widths(&transposed).iter().all(|w| *w <= 24),
            "{transposed:?}"
        );
    }

    /// Two runs there is nothing to lay out from, both of which fall back to the
    /// source rather than to silence.
    #[test]
    fn a_run_with_nothing_to_transpose_falls_back_to_its_source() {
        // A header with no body. Too narrow to align, and there are no data
        // rows to turn into blocks — emitting nothing would discard the header.
        let header_only = ["| Surface | Finding |", "|---------|---------|"];
        assert_eq!(layout_table(&header_only, 12), source_rows(&header_only));

        // A run the caller mis-buffered. The shape is not the one this function
        // was promised, and guessing at it is the one move ADR-2 rules out.
        let mixed = ["| a | b |", "not a table row at all"];
        assert_eq!(layout_table(&mixed, 40), source_rows(&mixed));
    }

    // ---- BR-10: the structural sweep ---------------------------------------

    /// **REQ-592 BR-10: this module decides, and never prints.**
    ///
    /// Modelled on `status.rs`'s `the_status_module_never_writes_to_a_terminal`,
    /// and here for the same reason: BR-10 says every layout decision is
    /// reachable without a terminal, and a module that quietly grew a
    /// `write!(self.out, …)` or a `terminal_width()` call would still look
    /// correct in a terminal while becoming invisible to the default test suite
    /// — LESSON-481's blindfold, with the gate on the other side.
    ///
    /// `terminal_width` is in the list alongside the write verbs because reading
    /// the width is the same failure as writing bytes: it makes the decision
    /// depend on an environment `cargo test` does not provide. The width is a
    /// parameter here, always.
    #[test]
    fn the_markdown_module_never_writes_to_a_terminal_or_reads_its_width() {
        let source = scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "markdown.rs")
            .map(|(_, src)| scan::code_only(&src))
            .expect("this module is a production source");

        // Non-vacuity: the scan is looking at this file and not at an empty
        // string, so the assertions below mean something.
        assert!(
            source.contains("pub fn wrap_ranges"),
            "the sweep is not reading markdown.rs any more"
        );

        for needle in [
            "print!",
            "println!",
            "eprint!",
            "eprintln!",
            "write!",
            "writeln!",
            "stdout",
            "stderr",
            "terminal_width",
        ] {
            assert!(
                !source.contains(needle),
                "markdown.rs names `{needle}`. Layout is a pure function of text and a \
                 width (BR-10): the width is a parameter, never queried, and the bytes go \
                 out through the Surface seam. A module that prints or reads the terminal \
                 is a module the default `cargo test` suite cannot drive."
            );
        }
    }
}
