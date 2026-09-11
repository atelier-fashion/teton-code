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
    /// The live turn-activity row: what the turn is doing right now, redrawn in
    /// place while it runs and taken back before anything durable prints
    /// (REQ-621 ADR-621-3).
    ///
    /// One of the two **transient** classes. Every durable kind here names a
    /// line the reader can scroll back to; this one is withdrawn rather than
    /// left behind (BR-5), which is why the trait needs a verb for un-drawing a
    /// row and not just for redrawing one.
    Activity,
    /// The live **pending** row: the line the user is typing during a turn,
    /// drawn beneath the activity row and left as the row the cursor sits on
    /// (REQ-622 ADR-622-4).
    ///
    /// A class of its own rather than a second use of [`LineKind::Activity`],
    /// and the difference is not cosmetic. The activity row is the client
    /// talking about the turn; this row is the *user's own text*, echoed back
    /// by the client because the kernel no longer does it. They are drawn by
    /// different verbs, at different places in the block, and — since a verb
    /// takes the class rather than the row's identity — a recorder or a future
    /// pane that could not tell them apart could not assert which row it was
    /// looking at.
    ///
    /// Its text is the editor's [`crate::input_editor::InputEditor::row`],
    /// which has already been through [`defused`]; so it is a **styled class
    /// with no carve-out** in the escape guard, unlike `Activity`, whose text
    /// interpolates a daemon-supplied tool title. If an escape ever reaches
    /// this class, something upstream stopped defusing and the debug build
    /// should say so.
    Pending,
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
    ///
    /// **Exhaustive, with no `_` arm** (REQ-622, verify). A wildcard here made
    /// "unstyled" the default a new class fell into silently; every variant now
    /// states its answer, so adding one is a decision the compiler asks for
    /// rather than a table it quietly extends.
    fn sgr(self) -> Option<&'static str> {
        match self {
            LineKind::BannerArt => Some("36"),
            LineKind::BannerTitle => Some("1"),
            LineKind::BannerMeta => Some("2"),
            // Dim, and `BannerMeta`'s "2" for `BannerMeta`'s reason: the row is
            // scaffolding around the turn's real output, not part of it. It is
            // also the one line on screen that is about to be taken back, so
            // drawing it at full weight would give the most temporary thing on
            // the terminal the most attention.
            LineKind::Activity => Some("2"),
            // Bold, and deliberately **not** the activity row's dim: this row
            // is the user's own sentence and the row their cursor is resting
            // on. Drawing what somebody is typing fainter than everything
            // around it is the one place on this screen where "temporary"
            // is the wrong reading — the text is going to be sent.
            LineKind::Pending => Some("1"),
            LineKind::Notice
            | LineKind::Tool
            | LineKind::Diff
            | LineKind::Prompt
            | LineKind::Cost
            | LineKind::Info
            | LineKind::Error => None,
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

    /// Draw one live row at the cursor — a row its owner will repaint in place
    /// and take back with [`Surface::withdraw_row_above`] — **holding whatever
    /// the surface is holding** (REQ-622 BR-4, BR-13).
    ///
    /// [`Surface::line`] with one difference, and the difference is the verb's
    /// reason to exist. A durable line owns its row for good, so the text the
    /// renderer is still holding — a streamed line no newline has completed
    /// yet (REQ-592) — goes out ahead of it, or the screen reads in the wrong
    /// order (BR-8). A live row is gone again before anything durable is
    /// written: the pump withdraws the block ahead of every durable write and
    /// redraws it after (REQ-621 ADR-621-3, REQ-622 ADR-622-4). Emitting the
    /// held text ahead of a live row therefore ends a line the stream had not
    /// ended — and the block is redrawn after every streamed token, so a reply
    /// streamed while a pending row was up reached the screen one token per
    /// row where the same reply with nothing typed was one row. Held text
    /// stays held across a live row's draw, its repaints and its withdraw, and
    /// is written where the block was by the durable write — or the turn's
    /// [`Surface::end_block`] — that follows the withdraw.
    ///
    /// **Defaults to [`Surface::line`]**, which is right for every surface
    /// that holds nothing: a recording double sees the same call a durable
    /// line makes, and a surface with no cursor writes the row as it writes
    /// any line. Only a surface that both holds text and owns live rows has
    /// the two verbs disagree, and there is one of those.
    fn draw_row(&mut self, kind: LineKind, text: &str) {
        self.line(kind, text);
    }

    /// Draw the block's **bottom** row at the cursor and leave the cursor at
    /// its end — no trailing newline (REQ-622 ADR-622-4).
    ///
    /// [`Surface::draw_row`] draws a row and steps past it, so the cursor ends
    /// up on the blank row *below* the block; that is right for the activity
    /// row, which the client is only talking over. It is wrong for the pending
    /// row, which is the line the user is typing: ADR-622-4 says the cursor
    /// rests at the end of that row, because a terminal draws its caret where
    /// the cursor is and a caret parked a row below the text is a caret that
    /// says "type here" about the wrong row.
    ///
    /// Holding is [`Surface::draw_row`]'s rule, for its reason (BR-4): this is
    /// a live row, gone again before anything durable is written.
    ///
    /// **Defaults to doing nothing**, where `draw_row` defaults to `line`. A
    /// row with no newline after it is not a line and cannot be written as one:
    /// a surface with no cursor would leave the next line appended to the
    /// user's half-typed sentence. Only a surface that owns live rows reaches
    /// this verb at all — the pump's pending row is gated on `owns_input`,
    /// which is gated on [`Surface::has_live_rows`] — so the default is
    /// unreachable in production and silence is the honest answer for the
    /// surfaces that have it.
    fn draw_current_row(&mut self, kind: LineKind, text: &str) {
        let _ = (kind, text);
    }

    /// Repaint the row the cursor is **on**, in place, leaving the cursor at
    /// its end (REQ-622 ADR-622-4).
    ///
    /// [`Surface::repaint_row_above`] without the offset and without the
    /// `\x1b[s` / `\x1b[u` pair, and the absence of that pair is the point: a
    /// save/restore exists to put the cursor back where it was, and here it is
    /// already where it belongs — at the end of the row just written. Carriage
    /// return, erase to end of line, write. Three bytes of escape rather than
    /// nine, and no dependence on a terminal's cursor-save register, which some
    /// multiplexers share between panes.
    ///
    /// Reports whether the bytes reached the terminal, [`Surface::
    /// repaint_row_above`]'s contract for its reason (BR-13). `false` from the
    /// default, which has written nothing.
    fn repaint_current_row(&mut self, kind: LineKind, text: &str) -> bool {
        let _ = (kind, text);
        false
    }

    /// Clear the row the cursor is **on** and leave the cursor at its start
    /// (REQ-622 ADR-622-4).
    ///
    /// [`Surface::withdraw_row_above`] with no cursor motion, because there is
    /// none to make. What it leaves behind is exactly the state the block was
    /// in before the pending row was drawn — the cursor at column 0 of an empty
    /// row directly under the activity row — so the offsets the rest of the
    /// block uses are unchanged by whether a pending row was ever up, and
    /// `at_line_start` is honest again.
    ///
    /// Reports whether the bytes landed. `false` from the default, which has
    /// written nothing.
    fn withdraw_current_row(&mut self) -> bool {
        false
    }

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
    ///
    /// **Returns whether the bytes were written and flushed** (REQ-621 BR-13).
    /// A terminal can refuse a write — a closed pty on a detached session, a
    /// full pipe, an `EIO` from a window that has gone away — and the caller's
    /// whole geometry rests on the assumption that its last paint landed: a
    /// row it believes is on screen is a row it will keep repainting and will
    /// finally try to withdraw, one row above wherever the cursor now is. So
    /// the failure is *reported* rather than swallowed, and the pump answers it
    /// by giving up the row for the rest of the turn. The default answer is
    /// `false` for the same reason the body is empty: a surface that wrote
    /// nothing has not written the row.
    fn repaint_row_above(&mut self, _rows_up: usize, _kind: LineKind, _text: &str) -> bool {
        false
    }

    /// Take back the row `rows_up` above the cursor: step up, clear it, and
    /// leave the cursor at column 0 of the row just cleared (REQ-621
    /// ADR-621-3).
    ///
    /// [`Surface::repaint_row_above`]'s counterpart, and a verb of its own
    /// rather than a repaint with an empty string because **a repainted empty
    /// row is still a row**. The live activity row must not reach scrollback
    /// (BR-5), so what the pump needs — at the end of a turn, and ahead of
    /// every durable line it prints while one is running — is the row gone.
    ///
    /// **The cursor is deliberately not restored**, which is the whole
    /// difference from `repaint_row_above`. That verb saves and restores
    /// because the row it redraws sits above text the user is typing. This one
    /// is removing a row so that something else can be written where it was, so
    /// leaving the cursor on the cleared row is the point: the caller's next
    /// `line()` lands there instead of one row lower, and no blank gap is left
    /// behind to be the residue this verb exists to avoid.
    ///
    /// `rows_up` comes from the caller for `repaint_row_above`'s reason: frame
    /// geometry is the caller's knowledge, the cursor is the surface's.
    ///
    /// **Defaults to a no-op**, which is BR-6's guarantee for every surface
    /// that is not a terminal — including any future one. A surface with no
    /// cursor has no row to take back, so silence is the correct behaviour
    /// rather than something each implementor must remember to add.
    ///
    /// **Returns whether the bytes were written and flushed** (REQ-621 BR-13),
    /// for the reason spelled out on [`Surface::repaint_row_above`]: a caller
    /// that cannot tell a cleared row from a refused write goes on believing
    /// it owns a row somebody else is now writing over. `false` from the
    /// default, which has written nothing.
    ///
    /// **Held text stays held.** A partial streamed line the renderer has not
    /// emitted yet (REQ-592) is not on screen, so it counts for nothing in
    /// `rows_up` and this verb leaves it exactly where it is — as does
    /// [`Surface::repaint_row_above`], and [`Surface::draw_row`] is the draw
    /// that matches. The held line is written where the row was, by the
    /// durable write or the turn's `end_block` that follows the withdraw
    /// (REQ-622 BR-4). Emitting it here instead ended a streamed line at every
    /// token that arrived while a row was up, which is the defect the pending
    /// row made visible.
    // This carried a `#[cfg_attr(not(test), expect(dead_code, …))]` until the
    // pump called it, chosen over an `allow` for the reason written out at
    // [`PlainSurface::with_markdown`]: an `allow` would have gone on being
    // correct once the caller landed and would have sat here forever, whereas
    // the `expect` *became* the warning the moment the pump reached the verb,
    // and `-D warnings` made deleting it a condition of landing that wiring.
    // The caller is `client.rs`'s turn pump, which owns the row (ADR-621-3).
    fn withdraw_row_above(&mut self, _rows_up: usize) -> bool {
        false
    }

    /// Whether this surface can carry a row that is drawn, repainted in place
    /// and then withdrawn — an animation the reader watches, rather than a line
    /// they scroll back to (REQ-621 ADR-621-3).
    ///
    /// **The TTY gate, held as a property of the surface rather than threaded
    /// through the callers.** The event pump reads this to decide whether to
    /// wait on its channel with a timeout at all (ADR-621-1): answered `false`
    /// it keeps the blocking `recv()` it has always had, never reaches a tick
    /// arm, and so cannot emit a byte it did not emit before. That is how BR-6's
    /// byte-identical piped output holds by construction instead of by a
    /// conditional at each site that draws — the same trade
    /// [`PlainSurface::with_markdown`] makes for the renderer (REQ-592 ADR-1).
    ///
    /// **Defaults to `false`.** A surface that has not said it can take a row
    /// back must not be given one, so a new implementor that never names this
    /// method inherits the silent answer rather than the animated one.
    fn has_live_rows(&self) -> bool {
        false
    }

    /// Emit anything the surface is still holding, and **touch no block state**
    /// (REQ-592 BR-8).
    ///
    /// The poll-safe half of [`Surface::end_block`], and the difference between
    /// the two is the whole reason there are two. Emitting held rows is
    /// something any caller about to claim the terminal may need at any moment;
    /// dropping block state is something only a caller that knows the *turn* is
    /// over may do. Fused into one verb, the second half rides along on every
    /// call site the first half needs — and the first half's call sites include
    /// a 120 ms poll loop.
    ///
    /// So this is the verb for a caller that is about to write, or about to hand
    /// the terminal to something that is not a `Surface` at all: the idle drain
    /// before it returns to a frame redraw, and the event pump before it lets
    /// [`crate::prompt::Prompter`] ask a permission question. It is the same
    /// thing `line()` and `repaint_row_above()` already do for themselves before
    /// their own row — named, so that a caller with no row of its own can ask
    /// for it without reaching for the turn-boundary verb.
    ///
    /// **Defaults to a no-op**, for `repaint_row_above`'s reason: a surface that
    /// holds nothing has nothing to emit, so silence is the correct behaviour
    /// rather than something each implementor must remember to add.
    fn emit_held(&mut self) {}

    /// Declare that the block of streamed output just ended: emit anything the
    /// surface is still holding, and drop whatever block state it accumulated
    /// while holding it (REQ-592 BR-8, ADR-3).
    ///
    /// [`Surface::emit_held`] plus the second half — and the second half is what
    /// this verb is *for*. A caller that only needs the buffer on screen wants
    /// `emit_held`; this one additionally forgets that a fence was open, which
    /// is a claim about the model's output and not about the terminal.
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
    /// (ADR-3, [[LESSON-547]]), and since REQ-592's verify there is exactly one:
    /// the end of `Connection::call`. Not `main.rs`, not `hand_off_after_turn`,
    /// not a self-flush in this module.
    ///
    /// The obvious site is `hand_off_after_turn`, and it is wrong — though not
    /// for the reason first recorded here, which was that a flush hung there
    /// would drop buffered text on every failed turn. That overstates it: every
    /// *arm* of `main.rs`'s turn match writes through [`Surface::line`] after
    /// `call` returns, and `line()` emits the held buffer ahead of its own row
    /// (BR-8), so on those paths the same bytes reach the screen in the same
    /// order either way — moving the flush there changes no pty output at all.
    ///
    /// Three things the hand-off still cannot do. It cannot **clear the fence
    /// bit**, the half only this verb performs: just the `Ok` arm of that match
    /// reaches it, so a turn the daemon refused mid-fence would leave
    /// `fence == true` and render every later reply of the session verbatim. It
    /// never runs when `call` returns through its own transport `?`, which
    /// leaves the entry loop without writing a line at all. And no arm of it
    /// runs on the idle path, where fragments arrive with no turn in flight. The
    /// pump is the one place that owns the surface on every path an event can
    /// take.
    ///
    /// **A turn boundary, and only a turn boundary.** This verb drops block
    /// state — the open-fence bit among it — because the *block* ended, so
    /// calling it at a mid-turn pause is a bug rather than a harmless extra
    /// flush. A model that opens a ` ```rust ` fence, hits a tool call, and
    /// resumes after the user answers the permission prompt would have the rest
    /// of its code classified as markdown and **word-wrapped at the terminal
    /// width** — one statement broken across rows at a space, and a wrapped
    /// shell command is a different command.
    ///
    /// The damage is re-flowed code, not stray emphasis: `**ptr` with no closing
    /// pair keeps its literal marker, and `*y * z` fails the space-flank rule, so
    /// [`markdown::parse_inline`] leaves both alone. Reaching for an emphasis
    /// example here would understate it — the wrap is the part that changes what
    /// the characters *mean*. That is BR-6's failure, caused by an over-eager
    /// call to the thing meant to prevent a different one. A mid-turn caller
    /// that needs the buffer on screen ahead of its own row has `line()`,
    /// `repaint_row_above()`, and [`Surface::emit_held`] — none of which touch
    /// the fence.
    ///
    /// **Defaults to a no-op**, for `repaint_row_above`'s reason: a surface that
    /// holds nothing has nothing to emit, so silence is the correct behaviour
    /// rather than something each implementor must remember to add. That default
    /// is also what keeps this verb from rippling through the ~15 modules that
    /// consume `&mut dyn Surface` and the three implementors that buffer
    /// nothing.
    fn end_block(&mut self) {}

    /// Tell the surface the terminal is now `width` columns wide (REQ-592 OQ-4).
    ///
    /// **A setter, never a query the surface makes for itself.** This is the
    /// same division `repaint_row_above` and `end_block` make, applied to the
    /// one runtime fact layout needs: `PlainSurface` is generic over its writer
    /// and [`crate::prompt::terminal_width`] reads `STDOUT_FILENO` specifically,
    /// so a surface over a `Vec<u8>` that asked for its own width would be
    /// measuring a terminal it is not writing to. Because the number always
    /// arrives from outside, every layout decision below stays reachable from a
    /// test with no terminal in sight — BR-10's rule, and the reason
    /// [`PlainSurface::with_markdown`] takes a width in the first place.
    ///
    /// **Rows already emitted keep the breaks they were laid out with.** Only
    /// blocks rendered after this call use the new width, which is exactly what
    /// OQ-4 decided: no `SIGWINCH` handler, a resize takes effect on the next
    /// block, and nothing already on screen is re-flowed. Text the surface is
    /// still *holding* has not been emitted, so it is laid out at the new width
    /// — the right answer, and the reason this deliberately does not flush.
    /// Flushing here would also be `end_block`'s job, which belongs to the event
    /// pump alone.
    ///
    /// **Defaults to a no-op**, for `end_block`'s reason: a surface that lays
    /// nothing out has no width, so silence is correct rather than something
    /// each implementor must remember to add. On a `PlainSurface` built without
    /// a renderer it is a no-op too — there is no width to change, which is BR-7
    /// holding here as it does everywhere else.
    fn set_width(&mut self, _width: usize) {}

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
/// these at the end of a turn is `Surface::end_block`, and its one call site
/// belongs to `client.rs`'s event pump (ADR-3) — this module must never decide
/// on its own that a block has ended. `Surface::emit_held` empties the first two
/// without touching `fence`, which is what a caller that merely needs the screen
/// current asks for.
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
    /// Whether this surface may carry a live row — one that is drawn, repainted
    /// in place, and then withdrawn (REQ-621 ADR-621-3).
    ///
    /// Its own field rather than a read of `markdown.is_some()`, which it
    /// currently tracks exactly, because the two are answers to different
    /// questions. Markdown is "lay this prose out"; this is "there is a
    /// terminal here whose rows can be taken back". A future surface over a
    /// terminal that renders no markdown wants the second without the first,
    /// and deriving one from the other would quietly hand it neither.
    ///
    /// Read only through [`Surface::has_live_rows`], whose one production
    /// caller is the turn pump's TTY gate (REQ-621 ADR-621-1).
    live_rows: bool,
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
    /// otherwise untouched, which is what every non-terminal target wants. It
    /// carries no live row either (REQ-621 BR-6) — colour is a property of the
    /// target, never a claim to own the target's rows.
    pub fn with_color(out: W, color: bool) -> Self {
        Self {
            out,
            at_line_start: true,
            color,
            markdown: None,
            live_rows: false,
        }
    }

    /// Wraps `out` in a surface that renders assistant text as markdown at
    /// `width` columns, styling it when `color` (REQ-592 BR-3..BR-6), and that
    /// carries live rows (REQ-621 BR-6).
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
    // construction — the place that owns the terminal gate, and the only place
    // that knows whether stdout is one (ADR-1, [[LESSON-547]]: a rule that
    // crosses a seam is owned by exactly one side).
    //
    // Until that caller existed this carried a `#[cfg_attr(not(test),
    // expect(dead_code, …))]`, chosen over an `allow` because an `allow` would
    // have gone on being correct once the wiring landed and would have sat here
    // forever — the lingering-suppression failure tetond's ADR-J is about. The
    // `expect` inverted it: the moment a real caller appeared the lint stopped
    // firing, *the attribute* became the warning, and `-D warnings` made
    // deleting it a condition of landing the gate. It did exactly that, and this
    // paragraph is what is left of it.
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
            // The one constructor that answers [`Surface::has_live_rows`] with
            // `true` (REQ-621 ADR-621-3), and for the reason written above: it
            // is chosen at the one place that knows stdout is a terminal. The
            // live row's TTY gate is therefore the same gate the renderer's is,
            // decided once, at construction, rather than re-derived by the
            // pump.
            live_rows: true,
        }
    }

    /// One row and its newline, styled by class: the bytes [`Surface::line`]
    /// and [`Surface::draw_row`] share, so the two cannot drift apart in
    /// anything but what they do *before* the row.
    ///
    /// The row itself is [`Self::styled_row`]'s, which is also what the
    /// current-row verbs write — so the four verbs that put a class on this
    /// surface differ in the cursor and in nothing else.
    fn write_line(&mut self, kind: LineKind, text: &str) {
        let row = self.styled_row(kind, text);

        // Close any open streamed line first so the row starts clean.
        if !self.at_line_start {
            let _ = writeln!(self.out);
        }
        let _ = writeln!(self.out, "{row}");
        self.at_line_start = true;
    }

    /// One row's bytes — prefix, defused text, and this class's styling — with
    /// **no newline**: what [`Self::write_line`] puts a newline after, and what
    /// the two current-row verbs write as-is (REQ-622 ADR-622-4).
    ///
    /// Split out so a row drawn as a line and the same row drawn as the row the
    /// cursor sits on cannot differ in anything but the newline. The escape
    /// guard lives here for the same reason: it has to hold for every verb that
    /// puts a styled class on the screen, not only for the one that ends a row.
    fn styled_row(&self, kind: LineKind, text: &str) -> String {
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
        //
        // `Activity` is styled and exempt, which is that flip side arriving by
        // a third route: the row's text is composed by this binary, but it
        // *interpolates* strings from outside — the daemon's tool title, which
        // carries model-proposed arguments, and a provider/model name (REQ-621
        // ADR-621-2). So an ESC reaching it is `Prompt`'s hostile input rather
        // than a caller reaching for SGR by hand, and a panic here would be the
        // guard handing a fetched page a debug-build crash. `defused` below is
        // what holds for it, in every build — BR-13: painting the row is never
        // fatal to the turn.
        //
        // `Pending` is styled and **not** exempt (REQ-622, verify). Its text is
        // the user's own keystrokes, and the editor has already defused them
        // before composing the row — so unlike `Activity` there is no outside
        // string interpolated into it, and an ESC arriving here would mean the
        // editor stopped defusing rather than that a fetched page tried
        // something. That is a defect to fail loudly on, which is what a
        // carve-out would have hidden.
        debug_assert!(
            kind.sgr().is_none() || kind == LineKind::Activity || !text.contains('\x1b'),
            "{kind:?} is styled by the surface; it must not carry its own escapes \
             (they will be neutralized into visible debris): {text:?}"
        );

        let body = defused(text);
        let prefix = Self::prefix(kind);
        match kind.sgr().filter(|_| self.color) {
            Some(sgr) => format!("\x1b[{sgr}m{prefix}{body}\x1b[0m"),
            None => format!("{prefix}{body}"),
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
            // No prefix: the frame composes its own leading glyph — the spinner
            // (ADR-621-2) — so a `>> ` here would shift the animation one
            // column right of where a repaint of the same row puts it.
            LineKind::Activity => "",
            // Nor here, for the same reason one row down: the editor composes
            // the `> ` marker itself and *measures the row against it*
            // ([`crate::input_editor::InputEditor::row`]), so a prefix added
            // here would be two columns the fit did not budget for — and a row
            // wider than the terminal hard-wraps into a second row the withdraw
            // cannot clear.
            LineKind::Pending => "",
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
    /// delimiter is recognized through [`markdown::fence_close`] — a
    /// *different* question from the `fence_open` one [`markdown::classify`]
    /// asks, and deliberately so. An opener may carry an info string
    /// (```` ```rust ````); a closer may not. Asking one function both
    /// questions is exactly what let a nested opener close its parent, which
    /// the verify pass found and this split fixes. A fence the two disagreed
    /// about would never close, and every remaining line of the reply would
    /// render as code.
    fn render_source_line(&mut self, line: &str) {
        if self.markdown.as_ref().is_some_and(|state| state.fence) {
            if markdown::fence_close(line).is_some() {
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
    /// This is not `end_block()`. That verb drops the fence bit too, and its one
    /// call site belongs to `client.rs`'s event pump at a turn boundary (ADR-3);
    /// what happens here is the narrow case where something is about to write
    /// and the buffer must go out ahead of it. It is also the whole of
    /// [`Surface::emit_held`] — the same narrow case, named for a caller that
    /// has no row of its own to write.
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
    /// A durable line: everything the renderer is holding, then the row. The
    /// styling and the escape guard are [`Self::write_line`]'s, shared with
    /// [`Surface::draw_row`].
    fn line(&mut self, kind: LineKind, text: &str) {
        // A line owns its row, so anything the renderer is still holding goes
        // out ahead of it (REQ-592 BR-8) — otherwise a notice arriving mid-turn
        // prints above a sentence the reader has not been shown yet, and the
        // screen reads in the wrong order. Inert without a renderer, and it
        // leaves the surface at the start of a row, so the close inside
        // `write_line` is unchanged for both paths.
        self.emit_pending();
        self.write_line(kind, text);
    }

    /// A live row: the row, and nothing held goes out ahead of it (REQ-622
    /// BR-4) — the whole of the difference from [`Surface::line`], written out
    /// on the trait.
    ///
    /// Two invariants asserted rather than handled, because on the one surface
    /// that reaches this verb they hold by construction and handling them would
    /// be code for a case that cannot arrive. Only [`Self::with_markdown`]
    /// answers `has_live_rows`, so a live row is only ever drawn over a
    /// renderer; and a renderer never leaves the cursor mid-row — every byte it
    /// emits goes through [`Self::write_row`], newline included — so the row
    /// beneath which this one is drawn is always complete, and taking the row
    /// back leaves the cursor at the start of an empty row where the held line
    /// will land whole. A surface that streamed fragments straight to the
    /// terminal *and* owned live rows would need to remember the column the
    /// open fragment reached and put the cursor back there after the withdraw;
    /// none exists, and this assertion is what says so rather than a paragraph
    /// nobody checks.
    fn draw_row(&mut self, kind: LineKind, text: &str) {
        debug_assert!(
            self.live_rows,
            "draw_row on a surface with no live rows: the pump's TTY gate is \
             read once per call, and it answered no"
        );
        debug_assert!(
            self.at_line_start,
            "draw_row with an open row on screen: a live surface holds partial \
             lines rather than streaming them, so the cursor is never mid-row here"
        );
        self.write_line(kind, text);
    }

    /// The block's bottom row, written where the cursor is and **not** ended
    /// (REQ-622 ADR-622-4). `at_line_start` goes false, which is the honest
    /// answer and is what the whole block's bookkeeping now rests on: a `line`
    /// that arrived with this row up would open with a newline rather than
    /// writing over the user's text.
    ///
    /// [`Surface::draw_row`]'s two assertions, for its reasons — and the second
    /// of them is what says this row is drawn onto a clean row rather than onto
    /// the end of another one.
    fn draw_current_row(&mut self, kind: LineKind, text: &str) {
        debug_assert!(
            self.live_rows,
            "draw_current_row on a surface with no live rows: the pump's TTY \
             gate is read once per call, and it answered no"
        );
        debug_assert!(
            self.at_line_start,
            "draw_current_row onto an open row: the block draws its bottom row \
             onto a clean row — either the one a `draw_row` above it just \
             ended, or the one a withdraw just cleared"
        );
        let row = self.styled_row(kind, text);
        let _ = write!(self.out, "{row}");
        let _ = self.out.flush();
        self.at_line_start = false;
    }

    /// `\r`, erase, write — no `\x1b[s` / `\x1b[u` pair, because the cursor is
    /// meant to end up at the end of this row and that is where writing it
    /// leaves it (REQ-622 ADR-622-4).
    ///
    /// `at_line_start` is deliberately untouched: it was false when this row
    /// went up and it is false now.
    ///
    /// Both halves of the write are checked, [`Surface::repaint_row_above`]'s
    /// rule for its reason — `write!` fills the buffer, the `flush` is what
    /// puts the bytes on the screen.
    fn repaint_current_row(&mut self, kind: LineKind, text: &str) -> bool {
        let row = self.styled_row(kind, text);
        write!(self.out, "\r\x1b[K{row}")
            .and_then(|()| self.out.flush())
            .is_ok()
    }

    /// Erase the row the cursor is on and leave it at column 0.
    ///
    /// Gated on `live_rows` and reporting its bytes, both for
    /// [`Surface::withdraw_row_above`]'s reasons. Held text stays held, also
    /// for its reason (BR-4).
    fn withdraw_current_row(&mut self) -> bool {
        if !self.live_rows {
            return false;
        }
        let written = write!(self.out, "\r\x1b[K")
            .and_then(|()| self.out.flush())
            .is_ok();
        self.at_line_start = true;
        written
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
    ///
    /// Reports whether the bytes reached the terminal (BR-13). Both halves are
    /// checked, because either failing leaves the row somewhere other than
    /// where the caller believes it is: `write!` puts the escapes in the
    /// buffer, and the `flush` is what puts them on the screen.
    fn repaint_row_above(&mut self, rows_up: usize, kind: LineKind, text: &str) -> bool {
        // Nothing held goes out here (REQ-622 BR-4). Held text is not on
        // screen, so it has no bearing on `rows_up`; and this verb runs with a
        // live row up, so emitting it *would* scroll the frame out from under
        // the row being repainted — the failure the old flush-first rule was
        // written against, arriving through the flush itself. The reasoning is
        // written out once, at `withdraw_row_above`.

        let prefix = Self::prefix(kind);
        // A repaint claims exactly one row, and the cursor restore assumes it: a
        // newline here would scroll the frame out from under `\x1b[u` and leave
        // the entry area shredded. That is the sharper consequence, but it is
        // not a different rule — [`defused`] is what `line()` uses too, for the
        // reason written there.
        let single_row = defused(text);
        write!(
            self.out,
            "\x1b[s\x1b[{rows_up}A\r\x1b[K{prefix}{single_row}\x1b[u"
        )
        .and_then(|()| self.out.flush())
        .is_ok()
    }

    /// Step up, clear the row, and stop. The cursor is left on the cleared row,
    /// so `at_line_start` is set: that is where the caller's next write lands,
    /// and a `line()` that thought it was mid-row would open with a newline and
    /// leave the blank gap this verb exists to close.
    ///
    /// **Gated on `live_rows`, where [`Surface::repaint_row_above`] above is
    /// not.** The asymmetry is deliberate and it is the newer half that is
    /// right. When the repaint verb landed (REQ-556) the surface did not know
    /// whether it was a terminal, so the gate had to sit at its callers; the
    /// surface holds that answer now, so this verb refuses on its own rather
    /// than trusting every future caller to ask first. BR-6 says the piped path
    /// emits *not a frame, not an escape, not a blank line* — the cheapest way
    /// to mean that is for the bytes to be unreachable, not merely unrequested.
    ///
    /// Reports whether the bytes reached the terminal (BR-13) —
    /// [`Surface::repaint_row_above`]'s rule, for its reason. A surface that
    /// refused the row on its own gate reports `false` too: nothing was
    /// written, which is the honest answer to "is that row gone", and the only
    /// caller that can reach this verb at all is one the gate answered `true`.
    fn withdraw_row_above(&mut self, rows_up: usize) -> bool {
        if !self.live_rows {
            return false;
        }

        // Held text stays held (REQ-622 BR-4). It is not on screen, so
        // `rows_up` — counted from the cursor to a row that *is* — owes it
        // nothing; and the row above the cursor is the one `draw_row` put
        // there, which held too. This verb used to emit the held line first, on
        // the argument that cursor motion must not be counted from a row the
        // reader has not been shown — but the held line was never shown and
        // never moved the cursor, and emitting it here ended a streamed line at
        // every token that arrived while a pending row was up: `One two three
        // four five.` reached the screen as five rows. What the held line waits
        // for is the durable write, or the turn's `end_block`, that follows
        // this withdraw — both land where the block was, which is where the
        // line would have gone with no block at all.

        // No `\x1b[s` / `\x1b[u` pair, unlike the repaint: the cursor is meant
        // to end up here.
        let written = write!(self.out, "\x1b[{rows_up}A\r\x1b[K")
            .and_then(|()| self.out.flush())
            .is_ok();
        self.at_line_start = true;
        written
    }

    /// The constructor's answer, unchanged since construction: only
    /// [`PlainSurface::with_markdown`] builds a surface that owns a terminal's
    /// rows (REQ-621 BR-6).
    fn has_live_rows(&self) -> bool {
        self.live_rows
    }

    /// The held rows, and nothing else — the same [`Self::emit_pending`] a
    /// mid-stream `line()` runs, exposed for a caller that has no row of its own.
    ///
    /// The fence bit is deliberately *not* touched here. That is the entire
    /// distinction from [`Surface::end_block`] below, and it is what makes this
    /// verb safe to call from a poll loop: the idle drain runs eight times a
    /// second, and a fence cleared at that rate reclassifies a broadcast code
    /// block as prose from the next poll onward.
    fn emit_held(&mut self) {
        self.emit_pending();
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

    /// No renderer, no width: the `if let` is the whole of BR-7's answer here.
    fn set_width(&mut self, width: usize) {
        if let Some(state) = self.markdown.as_mut() {
            state.width = width;
        }
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
    /// A `withdraw_row_above(rows_up)` call — recorded distinctly from
    /// `Repaint` because the two make opposite claims about scrollback: one
    /// leaves a row on screen, the other takes it back. A recorder that could
    /// not tell them apart could not assert the absence of residue, which is
    /// the property REQ-621 BR-5 is about.
    Withdraw(usize),
    /// A `draw_current_row(kind, text)` call — the block's bottom row, drawn
    /// with the cursor left at its end (REQ-622 ADR-622-4). Recorded
    /// distinctly from `Line` because that is the whole difference the verb
    /// exists for: a recorder that folded the two together could not tell a row
    /// the cursor is resting on from one it has stepped past, which is the
    /// geometry the pending row's every offset is measured against.
    DrawCurrent(LineKind, String),
    /// A `repaint_current_row(kind, text)` call — the same row rewritten in
    /// place, with no offset, because the cursor is already on it.
    RepaintCurrent(LineKind, String),
    /// A `withdraw_current_row()` call. No offset for the same reason.
    WithdrawCurrent,
}

/// A [`Surface`] that records every call instead of writing bytes. Test-only.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RecordingSurface {
    /// Every render call, in order.
    pub calls: Vec<Rendered>,
    /// The answer this recorder gives to [`Surface::has_live_rows`]. A flag
    /// rather than a terminal, so the TTY-gated path is reachable from a test
    /// with no terminal in sight (ADR-621-3).
    live_rows: bool,
    /// Whether the two row verbs report their bytes as **not** written
    /// (REQ-621 BR-13).
    ///
    /// A flag rather than a failing `Write`, because the two answer different
    /// questions. `PlainSurface` over a writer that returns `Err` is what pins
    /// the *reporting* — and `render.rs` tests it there — but a surface whose
    /// every write fails cannot record the verbose line BR-13 requires the
    /// client to print, so the failure would be unobservable at the seam that
    /// has to react to it. Here the row verbs fail and `line` still records,
    /// which is the terminal this rule is actually about: one that took the
    /// prose and refused the cursor motion.
    failing_rows: bool,
}

#[cfg(test)]
impl RecordingSurface {
    /// A fresh recorder. Answers [`Surface::has_live_rows`] with `false`, like
    /// the piped surface it stands in for.
    pub fn new() -> Self {
        Self::default()
    }

    /// A recorder that claims live rows, for a test that has to drive the
    /// pump's TTY-gated path (ADR-621-3) without a terminal to gate on.
    pub fn with_live_rows() -> Self {
        Self {
            live_rows: true,
            ..Self::default()
        }
    }

    /// A recorder that claims live rows and then **refuses** every row verb,
    /// for BR-13's path: a terminal that will not take the row's bytes.
    ///
    /// The attempt is still recorded. What the caller did and what the terminal
    /// took are two facts, and a recorder that dropped the call could not tell
    /// "the pump stopped painting" from "the pump never painted".
    pub fn with_failing_rows() -> Self {
        Self {
            live_rows: true,
            failing_rows: true,
            ..Self::default()
        }
    }

    /// The concatenation of every fragment written (the streamed assistant text).
    pub fn fragments(&self) -> String {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Rendered::Fragment(t) => Some(t.as_str()),
                Rendered::Line(..)
                | Rendered::Repaint(..)
                | Rendered::Withdraw(_)
                | Rendered::DrawCurrent(..)
                | Rendered::RepaintCurrent(..)
                | Rendered::WithdrawCurrent => None,
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

    fn repaint_row_above(&mut self, rows_up: usize, kind: LineKind, text: &str) -> bool {
        self.calls
            .push(Rendered::Repaint(rows_up, kind, text.to_owned()));
        !self.failing_rows
    }

    fn withdraw_row_above(&mut self, rows_up: usize) -> bool {
        self.calls.push(Rendered::Withdraw(rows_up));
        !self.failing_rows
    }

    fn draw_current_row(&mut self, kind: LineKind, text: &str) {
        self.calls
            .push(Rendered::DrawCurrent(kind, text.to_owned()));
    }

    fn repaint_current_row(&mut self, kind: LineKind, text: &str) -> bool {
        self.calls
            .push(Rendered::RepaintCurrent(kind, text.to_owned()));
        !self.failing_rows
    }

    fn withdraw_current_row(&mut self) -> bool {
        self.calls.push(Rendered::WithdrawCurrent);
        !self.failing_rows
    }

    fn has_live_rows(&self) -> bool {
        self.live_rows
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

    /// **REQ-622 ADR-622-4, at the bytes: the pending row is the row the cursor
    /// is on.**
    ///
    /// One literal byte string for the whole block, because what is under test
    /// is an *order and a cursor*, and a computed expectation would reproduce
    /// whatever order the code chose ([[LESSON-569]]). Read left to right it is
    /// the activity row **and its newline** — the cursor steps past that row —
    /// then the pending row with **no** newline, so the terminal's caret is left
    /// immediately after `l`; then a repaint that is `\r`, erase, rewrite and
    /// nothing else — no `\x1b[s` / `\x1b[u`, because the cursor is meant to end
    /// up exactly where writing leaves it; then a withdraw that clears that row
    /// and leaves the cursor at its start.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** give
    /// `PlainSurface::draw_current_row` a trailing newline (`write!` →
    /// `writeln!`). **1 red of 872** — this test, on the `> hal\r` the oracle
    /// expects where `> hal\n\r` arrives. At a real terminal the caret would
    /// then sit on the blank row *below* the sentence the user is typing, and
    /// every offset above it would be one row short — which is the whole of
    /// what BUG-225 was. Reverted with the same edit.
    ///
    /// **That it is one red and not three is the finding.** Neither
    /// `a_current_rows_bookkeeping_is_honest` below nor
    /// `client.rs`'s `the_pending_row_is_the_row_the_cursor_is_on` notices: the
    /// first asserts the `at_line_start` *field*, which the mutant still sets to
    /// `false` — the field goes on agreeing with the code and stops agreeing
    /// with the terminal — and the second records verbs rather than bytes, so a
    /// `DrawCurrent` that emitted a newline is indistinguishable there from one
    /// that did not. A recorder cannot see a cursor. This literal byte string is
    /// the only oracle in the suite that can, which is why it is one.
    #[test]
    fn a_current_row_is_drawn_unterminated_and_repainted_where_the_cursor_is() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
            surface.draw_row(LineKind::Activity, "⠋ preparing turn");
            surface.draw_current_row(LineKind::Pending, "> hal");
            assert!(surface.repaint_current_row(LineKind::Pending, "> half"));
            assert!(surface.withdraw_current_row());
        }
        assert_eq!(
            String::from_utf8(buf).expect("utf-8"),
            "⠋ preparing turn\n> hal\r\x1b[K> half\r\x1b[K",
        );
    }

    /// `at_line_start` is the surface's claim about where the cursor is, and the
    /// current-row verbs are the only ones that can make it false on purpose.
    ///
    /// The claim matters because `line()` reads it: a durable line written while
    /// the pending row is up has to open with a newline rather than writing over
    /// the user's own text. So this asserts the field **and** the byte that
    /// depends on it.
    #[test]
    fn a_current_rows_bookkeeping_is_honest() {
        let mut buf = Vec::new();
        let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
        assert!(surface.at_line_start, "a fresh surface is at a row's start");

        surface.draw_current_row(LineKind::Pending, "> half");
        assert!(
            !surface.at_line_start,
            "the cursor is at the end of the user's text, not at a row's start"
        );
        surface.repaint_current_row(LineKind::Pending, "> half a");
        assert!(!surface.at_line_start, "and a repaint leaves it there");

        surface.line(LineKind::Info, "a notice");
        assert!(surface.at_line_start);
        drop(surface);
        let out = String::from_utf8(buf).expect("utf-8");
        assert!(
            out.ends_with("\na notice\n"),
            "a durable line written over a current row opens its own row rather \
             than appending to what the user is typing: {out:?}"
        );

        let mut buf = Vec::new();
        let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
        surface.draw_current_row(LineKind::Pending, "> half");
        assert!(surface.withdraw_current_row());
        assert!(
            surface.at_line_start,
            "the withdraw leaves the cursor at column 0 of the row it cleared, \
             which is where the activity row's `withdraw_row_above(1)` measures \
             from"
        );
    }

    /// A surface with no cursor **declines** the current row rather than writing
    /// it as a line (REQ-622 ADR-622-4).
    ///
    /// The default is the interesting half. [`Surface::draw_row`] defaults to
    /// `line`, which is right — a row and a line differ only in who takes the
    /// row back. A row with no newline after it is a different thing: written
    /// as a line to a log or a pipe it would leave the next line appended to the
    /// user's half-typed sentence. So the default writes nothing and the two
    /// fallible verbs report `false`, and the pump never reaches them because
    /// the pending row is gated on a surface that answers `has_live_rows`.
    #[test]
    fn a_surface_with_no_cursor_declines_the_current_row() {
        #[derive(Default)]
        struct Bare(Vec<String>);
        impl Surface for Bare {
            fn line(&mut self, _kind: LineKind, text: &str) {
                self.0.push(text.to_owned());
            }
            fn fragment(&mut self, _text: &str) {}
        }

        let mut bare = Bare::default();
        bare.draw_row(LineKind::Activity, "a live row");
        bare.draw_current_row(LineKind::Pending, "> typing");
        assert!(!bare.repaint_current_row(LineKind::Pending, "> typing on"));
        assert!(!bare.withdraw_current_row());
        assert_eq!(
            bare.0,
            vec!["a live row".to_owned()],
            "`draw_row` falls through to `line` and the three current-row verbs \
             write nothing at all"
        );
    }

    /// **REQ-622, verify: `LineKind::Pending` is a class of its own, styled, and
    /// not the activity row's dim.**
    ///
    /// The pending row is the user's own sentence and the row their cursor is
    /// resting on. Drawn at the activity row's dim it would be the faintest
    /// thing on a screen that is mostly the model's output, which is exactly
    /// backwards for the one line on it that the user wrote.
    #[test]
    fn the_pending_class_is_styled_and_is_not_the_activity_rows_dim() {
        let pending = rendered(true, LineKind::Pending, "> half a thought");
        assert!(
            pending.starts_with("\x1b[1m") && pending.ends_with("\x1b[0m\n"),
            "bold, opened and closed: {pending:?}"
        );
        assert!(!pending.contains("\x1b[2m"), "and never dim: {pending:?}");
        assert!(
            rendered(true, LineKind::Activity, "⠋ preparing turn").contains("\x1b[2m"),
            "while the row above it still is — the two classes are \
             distinguishable on the screen and not only in the source"
        );
        assert_eq!(
            rendered(false, LineKind::Pending, "> half a thought"),
            "> half a thought\n",
            "and the colour gate is the surface's for this class like every \
             other: no escape, and no literal `[1m` debris standing in for one"
        );
    }

    /// **REQ-622, verify: `Pending` is styled and has *no carve-out* in the
    /// escape guard.**
    ///
    /// [`LineKind::Activity`] is exempt because its text interpolates a
    /// daemon-supplied tool title, so an ESC reaching it is hostile input the
    /// guard is meant to neutralize rather than a caller reaching for SGR by
    /// hand. Nothing outside this binary is interpolated into a pending row:
    /// the editor composes it from the user's keystrokes and defuses them on
    /// the way out ([`crate::input_editor::InputEditor::row`]). So an ESC
    /// arriving here means something upstream stopped defusing — a defect to
    /// fail loudly on, and the thing a carve-out would have hidden.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** add `|| kind ==
    /// LineKind::Pending` to `styled_row`'s `debug_assert`. **1 red of 872**,
    /// this test, which then renders the escape as visible `[31m` debris
    /// instead of panicking. Reverted with the same edit.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "must not carry its own escapes")]
    fn a_pending_row_carrying_an_escape_is_a_defect_not_a_carve_out() {
        let _ = rendered(true, LineKind::Pending, "> \x1b[31mred");
    }

    /// The other half of the rule above, so it is a *choice* and not a blanket
    /// ban: the activity row's carve-out survives, because the string that
    /// reaches it is partly the daemon's.
    #[test]
    fn the_activity_rows_carve_out_survives() {
        let out = rendered(true, LineKind::Activity, "⠋ running shell: \x1b[31mrm");
        assert!(
            !out.contains("\x1b[31m"),
            "the escape is neutralized rather than panicked on: {out:?}"
        );
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

    /// REQ-621 BR-6 / ADR-621-3. The TTY gate the event pump reads is a
    /// property of the surface, and exactly one constructor sets it: the one
    /// `main.rs` picks when stdout is a terminal. A piped session builds one of
    /// the other two, so it never reaches the pump's tick arm and cannot emit a
    /// byte it did not emit before.
    #[test]
    fn only_the_markdown_surface_has_live_rows() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(
            !PlainSurface::new(&mut buf).has_live_rows(),
            "the plain constructor is the piped path"
        );
        assert!(
            !PlainSurface::with_color(&mut buf, true).has_live_rows(),
            "colour is a property of the target, not a claim to own its rows"
        );
        assert!(
            PlainSurface::with_markdown(&mut buf, true, 80).has_live_rows(),
            "the interactive constructor is the one that owns a terminal's rows"
        );
        assert!(
            !RecordingSurface::new().has_live_rows(),
            "the recorder defaults to the piped answer"
        );
        assert!(
            RecordingSurface::with_live_rows().has_live_rows(),
            "and opts in explicitly, so the gated path needs no terminal"
        );
    }

    /// **REQ-621 BR-13, at the seam that knows.** A row verb reports whether
    /// its bytes reached the terminal, so the one caller whose geometry depends
    /// on that — the pump, which believes a row is one above the cursor
    /// precisely because its last paint landed — can stop believing it.
    ///
    /// Both halves of the write are checked, and the test says so by failing
    /// each separately: `write!` puts the escapes in the buffer and `flush` is
    /// what puts them on the screen, and a caller told `true` by a surface that
    /// only buffered them is a caller told the wrong thing.
    ///
    /// The default answer is pinned here too. `false` is the honest report from
    /// a surface that wrote nothing, and it is what makes BR-13 hold for a
    /// future front-end that implements neither verb: the pump gives the row up
    /// rather than animating into a surface that is silently dropping it.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** return `true`
    /// unconditionally from `PlainSurface::repaint_row_above` (`let _ =
    /// write!(..); let _ = flush(); true`). **1 red of 828**, this test, on the
    /// failing-writer legs — and nothing else in the suite notices, because the
    /// client's own BR-13 test drives a recording surface. This is the only
    /// place the production writer's report is asserted at all. Reverted with
    /// the same edit.
    #[test]
    fn a_row_verb_reports_whether_its_bytes_landed() {
        /// A writer that refuses `write` (`kind` chooses which half fails).
        struct Refusing {
            on_write: bool,
        }
        impl Write for Refusing {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.on_write {
                    Err(std::io::Error::other("the pty is gone"))
                } else {
                    Ok(buf.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if self.on_write {
                    Ok(())
                } else {
                    Err(std::io::Error::other("the pty is gone"))
                }
            }
        }

        // A writer that takes the bytes: both verbs report the row landed.
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
            assert!(
                surface.repaint_row_above(1, LineKind::Activity, "⠋ preparing turn"),
                "a repaint whose bytes were written and flushed reports so"
            );
            assert!(surface.withdraw_row_above(1), "and so does a withdraw");
        }
        assert!(!buf.is_empty(), "the fixture really did write something");

        // A writer that refuses either half: both verbs report the row did not.
        for on_write in [true, false] {
            let mut surface = PlainSurface::with_markdown(Refusing { on_write }, false, 80);
            assert!(
                !surface.repaint_row_above(1, LineKind::Activity, "⠋ preparing turn"),
                "a repaint that could not reach the terminal must not report \
                 success (failing on_write={on_write})"
            );
            assert!(
                !surface.withdraw_row_above(1),
                "and neither must a withdraw (failing on_write={on_write})"
            );
        }

        // The defaults, which is what a surface that owns no cursor answers.
        struct Bare;
        impl Surface for Bare {
            fn line(&mut self, _kind: LineKind, _text: &str) {}
            fn fragment(&mut self, _text: &str) {}
        }
        let mut bare = Bare;
        assert!(
            !bare.repaint_row_above(1, LineKind::Activity, "⠋ preparing turn"),
            "a surface that wrote nothing has not written the row"
        );
        assert!(!bare.withdraw_row_above(1), "nor taken one back");

        // ...and the gate is the other reason a `PlainSurface` says `false`: a
        // piped one refuses the verb before it reaches a writer at all.
        let mut piped: Vec<u8> = Vec::new();
        assert!(
            !PlainSurface::new(&mut piped).withdraw_row_above(1),
            "a surface with no live rows has no row to report gone"
        );
        assert!(piped.is_empty(), "and it wrote nothing doing it");
    }

    /// REQ-622 BR-4, retiring the REQ-621 test that stood here. That test
    /// pinned the opposite order — the held row emitted *before* the cursor
    /// moved — on the argument that cursor motion must not be counted from a
    /// row the reader has not been shown. But a held row has not been shown
    /// and has not moved the cursor either: `rows_up` is counted from the
    /// cursor to the live row above it, which is on screen, and the held text
    /// is nowhere in that arithmetic. What the flush-first order did do was end
    /// a streamed line at every message that arrived while a row was up — and
    /// with REQ-622's pending row up for the whole of a stream, that was every
    /// token. The old test's own record explains why nothing caught it: "the
    /// pump withdraws the row before the reply's first byte and again before
    /// each durable line, which are all line boundaries" — true until a row was
    /// due mid-stream. The pty leg that saw it is
    /// `a_reply_streamed_past_a_pending_row_renders_as_it_does_without_one`.
    ///
    /// The oracle is the literal byte string, for the reason the old test gave
    /// ([[LESSON-569]]): the property is an *order*, and a computed expectation
    /// would reproduce whatever order the code chose. The held row goes out
    /// where the cleared row was, by the flush that follows.
    #[test]
    fn a_withdraw_leaves_the_held_row_held() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 40);
            // No trailing newline, so the renderer is *holding* this row.
            surface.fragment("held row");
            surface.withdraw_row_above(1);
            surface.end_block();
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\x1b[1A\r\x1b[Kheld row\n",
            "the withdraw is nothing but the step-up and the clear, and the held \
             row lands where the cleared row was, by the flush that follows"
        );
    }

    /// REQ-622 BR-4 / BR-13, the defect in the small. A reply streamed one
    /// token at a time while a pending row is up: each token arrives with the
    /// block withdrawn, is held, and the block is drawn again beneath and
    /// repainted once before the next token — and the line must reach the
    /// screen exactly as it would with no block at all: once, whole, where the
    /// block was, when the turn ends. `draw_row` is the verb that makes it so.
    /// The same sequence through `line` is the second assertion, which is what
    /// the pump did until REQ-622 and what the pty leg saw as five rows: a
    /// durable line flushes what is held, and that is right for a durable
    /// line.
    #[test]
    fn a_live_row_holds_what_the_renderer_is_holding() {
        fn stream(draw: fn(&mut PlainSurface<&mut Vec<u8>>, &str)) -> String {
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut surface = PlainSurface::with_markdown(&mut buf, false, 40);
                for token in ["One ", "two ", "three ", "four ", "five."] {
                    surface.fragment(token);
                    draw(&mut surface, "> typed");
                    surface.repaint_row_above(1, LineKind::Activity, "> typed a");
                    surface.withdraw_row_above(1);
                }
                surface.end_block();
            }
            String::from_utf8(buf).unwrap()
        }
        let cycle = "> typed\n\x1b[s\x1b[1A\r\x1b[K> typed a\x1b[u\x1b[1A\r\x1b[K";
        let held = stream(|surface, row| surface.draw_row(LineKind::Activity, row));
        assert_eq!(
            held,
            format!("{}One two three four five.\n", cycle.repeat(5)),
            "five cycles of draw, repaint and withdraw, and the reply once, \
             whole, where the last withdraw left the cursor: {held:?}"
        );
        let flushed = stream(|surface, row| surface.line(LineKind::Activity, row));
        assert!(
            flushed.starts_with("One\n> typed\n") && flushed.contains("\x1b[Ktwo\n> typed\n"),
            "a durable line flushes the held token ahead of itself, so the same \
             stream drawn through `line` is one row per token — the defect, and \
             the reason the block has a verb of its own: {flushed:?}"
        );
    }

    /// The same question asked of the **current-row** family (a53e9a6): a
    /// pending row drawn onto the cursor's row, repainted in place, and cleared,
    /// five times across a streamed reply, must leave the reply whole. Without
    /// this case the held-line property of `draw_current_row` was pinned only by
    /// a pty leg (`a_reply_streamed_past_a_pending_row_renders_as_it_does_without_one`).
    ///
    /// Mutation (applied, observed, reverted, 2026-09-11): `self.emit_pending();`
    /// at the head of each current-row verb in turn, the same edit that was
    /// measured against the feature branch before this case existed and left
    /// **0 red of 872** there while reddening 10 of 54 pty legs.
    ///
    /// | mutant                              | unit binary      |
    /// |-------------------------------------|------------------|
    /// | `draw_current_row` emits first      | **1 red of 874** |
    /// | `repaint_current_row` emits first   | **1 red of 874** |
    /// | `withdraw_current_row` emits first  | **1 red of 874** |
    ///
    /// This test is the one red every time — the reply arrives one token per
    /// row ahead of each draw, repaint or clear — and nothing else in the
    /// binary moves, so the held-line property of all three verbs now rests
    /// here rather than only on the pty suite. Reverted with the same edit.
    #[test]
    fn a_current_row_holds_what_the_renderer_is_holding() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 40);
            for token in ["One ", "two ", "three ", "four ", "five."] {
                surface.fragment(token);
                surface.draw_current_row(LineKind::Pending, "> typed");
                assert!(surface.repaint_current_row(LineKind::Pending, "> typed a"));
                assert!(surface.withdraw_current_row());
            }
            surface.end_block();
        }
        let held = String::from_utf8(buf).unwrap();
        let cycle = "> typed\r\x1b[K> typed a\r\x1b[K";
        assert_eq!(
            held,
            format!("{}One two three four five.\n", cycle.repeat(5)),
            "five cycles on the cursor's own row, and the reply once, whole, \
             where the last clear left the cursor: {held:?}"
        );
    }

    /// BR-6 twice over, as a claim about bytes: the constructors that do not own
    /// a terminal answer `false`, and the verb writes nothing for them even when
    /// a caller asks. "Not a frame, not an escape, not a blank line" is a
    /// byte-level promise, so it is asserted as one.
    #[test]
    fn a_surface_with_no_live_rows_withdraws_nothing() {
        for color in [false, true] {
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut surface = PlainSurface::with_color(&mut buf, color);
                surface.fragment("streamed");
                surface.withdraw_row_above(1);
            }
            assert_eq!(
                String::from_utf8(buf).unwrap(),
                "streamed",
                "a withdraw on a surface with no live rows must add no bytes"
            );
        }
    }

    /// BR-6 and BR-13's guarantee for every surface that is not a terminal,
    /// including future ones — `a_surface_that_does_not_override_repaint_emits_nothing`'s
    /// sibling for the verb that takes a row back. A new `Surface` implementor
    /// cannot forget to suppress the live row: silence and `false` are what it
    /// inherits by not writing either method.
    #[test]
    fn a_surface_that_does_not_override_withdraw_emits_nothing() {
        struct Bare(Vec<String>);
        impl Surface for Bare {
            fn line(&mut self, _kind: LineKind, text: &str) {
                self.0.push(text.to_owned());
            }
            fn fragment(&mut self, _text: &str) {}
        }
        let mut bare = Bare(Vec::new());
        bare.withdraw_row_above(1);
        assert!(
            bare.0.is_empty(),
            "the default withdraw must be a no-op: {:?}",
            bare.0
        );
        assert!(
            !bare.has_live_rows(),
            "and it must not claim rows it has no way to take back"
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

    /// **BR-3 and BR-5 at the same byte: an inline run that *straddles* a wrap
    /// break.**
    ///
    /// Every other styling test here either styles a single unbreakable word or
    /// wraps a paragraph that carries no markers, so none of them ever asks what
    /// happens when a run is still open at the end of a row. A terminal has no
    /// notion of "this attribute continues on the next line" — SGR is a stream,
    /// and a row that leaves bold open leaks it onto whatever is written next,
    /// including the entry frame and the next turn's notices. So the run has to
    /// be closed at the break and re-opened on the row after it, which is a
    /// property of [`styled_span`] being called once per wrapped span rather
    /// than once per run.
    ///
    /// Asserted as exact bytes rather than as "contains bold somewhere",
    /// because the failure this is aimed at is an *absent reset* — and a
    /// `contains` for text that is present either way cannot see one.
    ///
    /// (Verified by mutation: making `styled_span`'s post-loop reset fire only
    /// when `base.is_some()` — so an in-progress inline run is not closed at a
    /// row boundary — leaves the rest of the suite green and fails here.)
    #[test]
    fn a_styled_run_that_straddles_a_wrap_break_is_closed_and_reopened_per_row() {
        let out = markdown_out_ended(true, 20, &["**alpha bravo charlie delta echo**\n"]);

        assert_eq!(
            out, "\x1b[1malpha bravo charlie\x1b[0m\n\x1b[1mdelta echo\x1b[0m\n",
            "a run open at a row boundary must be closed there and re-opened on \
             the next row: {out:?}"
        );

        // Said again as the invariant rather than as the string, so the reason
        // survives a future width change: no row ends inside an open attribute.
        for row in out.lines() {
            assert!(
                !row.contains('\x1b') || row.ends_with(RESET),
                "a row that opened an attribute did not close it, so the \
                 attribute leaks onto the next thing written: {row:?}"
            );
        }
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

    /// REQ-622 BR-4. A repaint runs with a live row on screen, and buffered
    /// text is text that is *not* on screen: it stays held, and goes out only
    /// once the row is gone and something durable — or the turn's end — writes
    /// where the row was. Emitting it here would scroll the frame out from
    /// under the row being repainted, and it is what used to end a streamed
    /// line at every tick that fell mid-token while a pending row was up.
    /// (Until REQ-622 this test pinned the opposite order.)
    #[test]
    fn a_repaint_leaves_the_pending_buffer_held() {
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 80);
            surface.fragment("partially streamed");
            surface.repaint_row_above(2, LineKind::Notice, "model starting..");
            surface.end_block();
        }
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out, "\x1b[s\x1b[2A\r\x1b[K>> model starting..\x1b[upartially streamed\n",
            "the repaint is the save, the move, the clear, the row and the \
             restore, and the buffered row is still the renderer's to emit \
             afterwards: {out:?}"
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
    /// ordering property). It was dropped for exactly this. What stands there
    /// now is [`Surface::emit_held`] — the flush without the block-ending half —
    /// so the ordering is owned by the pump rather than by whatever
    /// `resolve_permission` happens to render first, and this test still holds.
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

    /// **OQ-4.** A resize takes effect on the next block, and the rows already
    /// on screen keep the breaks they were laid out with.
    ///
    /// The same sentence twice, with a narrower width declared in between. The
    /// first copy is a single 43-column row because it fits sixty; the second is
    /// three rows because it does not fit twenty. Asserted as one exact string
    /// rather than as two `contains`, because "the earlier output is untouched"
    /// is a claim about bytes that are *no longer reachable* — once emitted they
    /// cannot be re-flowed, and an equality is what says so.
    ///
    /// This is the defect the width-at-construction version shipped: a session
    /// started at 200 columns and dragged down to 80 went on laying every
    /// subsequent block out at 200, and the terminal then hard-wrapped those rows
    /// mid-word — which is precisely the defect REQ-592 exists to remove,
    /// returning for the rest of the session.
    #[test]
    fn set_width_lays_the_next_block_out_narrower_and_leaves_printed_rows_alone() {
        let sentence = "the quick brown fox jumps over the lazy dog\n";
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_markdown(&mut buf, false, 60);
            surface.fragment(sentence);
            surface.set_width(20);
            surface.fragment(sentence);
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "the quick brown fox jumps over the lazy dog\n\
             the quick brown fox\n\
             jumps over the lazy\n\
             dog\n",
            "the block after the resize must wrap at 20, and the one before it \
             must still be the single row it was emitted as"
        );
    }

    /// **BR-7, the other half of the new verb.** `set_width` on a surface with no
    /// renderer changes nothing — it cannot switch one on.
    ///
    /// Worth its own test because the failure would be silent and would land
    /// exactly where BR-7 promises it cannot: a piped session whose width
    /// happened to be set would start wrapping the model's bytes. The `if let`
    /// in the implementation is the whole guarantee, and this is what holds it
    /// there.
    #[test]
    fn set_width_cannot_give_a_pipe_a_renderer() {
        let sentence = "the quick brown fox jumps over the lazy dog\n";
        let mut buf = Vec::new();
        {
            let mut surface = PlainSurface::with_color(&mut buf, false);
            surface.set_width(20);
            surface.fragment(sentence);
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            sentence,
            "a surface with no renderer must pass the bytes through whatever it \
             has been told about the terminal"
        );
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

    /// **BR-7's other half: the renderer is chosen in exactly one place, and
    /// that place has a terminal in hand** (REQ-592 confirmation review,
    /// MAJOR 3).
    ///
    /// `requirement.md` says outright that AC-7 — one `cli_e2e` byte-comparison
    /// on a piped turn — is *the entire guard* for BR-7, and it measured the
    /// gap it was worried about: inverting the gate to always render leaves all
    /// 75 pre-existing `cli_e2e` tests green. Meanwhile the far smaller rule
    /// next door (which function may call `end_block`) had a source sweep with
    /// counts and region checks. This is that shape applied where it is
    /// actually load-bearing.
    ///
    /// Three properties, and each one is a real mutation:
    ///
    /// 1. **One** production call site. A second surface built with a renderer
    ///    somewhere else — a subcommand, a walkthrough, a diagnostic — is a
    ///    second answer to "is there a terminal", and half of them would be
    ///    wrong on a pipe.
    /// 2. It is in `main.rs`, the one edge that owns terminal facts (REQ-555,
    ///    REQ-585 BR-11: a handler must never read `IsTerminal` itself).
    /// 3. It is in a function that **also names `is_terminal`**, which is the
    ///    weakest honest statement of "gated": a sweep cannot read a
    ///    conditional, but a construction site that never mentions the terminal
    ///    at all is unambiguously ungated.
    #[test]
    fn the_markdown_renderer_is_constructed_once_and_behind_a_terminal_check() {
        let sources = crate::status::scan::production_sources();

        // `fn with_markdown` is the declaration, and it lives here by design;
        // every other occurrence is somebody choosing to render markdown.
        let code_by_file: Vec<(String, String)> = sources
            .iter()
            .map(|(rel, src)| {
                (
                    rel.clone(),
                    crate::status::scan::code_only(src).replace("fn with_markdown(", ""),
                )
            })
            .collect();
        let sites: Vec<&str> = code_by_file
            .iter()
            .flat_map(|(rel, code)| {
                std::iter::repeat_n(rel.as_str(), code.matches("with_markdown(").count())
            })
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "the markdown renderer is attached in exactly **one** production \
             place, because BR-7 is the question \"is there a terminal to lay \
             text out in\" and a second construction site is a second answer to \
             it. Sites found: {sites:?}"
        );
        assert_eq!(
            sites[0], "main.rs",
            "the gate belongs to the one edge that reads terminal facts; a \
             handler that decided this for itself would be a second, invisible \
             seam (REQ-585 BR-11)"
        );

        // The enclosing function, taken as the span from the last `fn ` before
        // the call to the next `fn ` after it. Coarse on purpose: it only has to
        // be tight enough that "this function never mentions the terminal" is a
        // true statement about the construction site.
        let code = &code_by_file
            .iter()
            .find(|(rel, _)| rel == "main.rs")
            .expect("main.rs is a production source")
            .1;
        let at = code.find("with_markdown(").expect("the site just counted");
        let opens = code[..at]
            .rfind("\nfn ")
            .expect("the site sits in a function");
        let closes = code[at..].find("\nfn ").map_or(code.len(), |off| at + off);
        assert!(
            code[opens..closes].contains("is_terminal"),
            "the one construction site must sit in a function that has actually \
             asked whether there is a terminal. This cannot read the conditional \
             — but a site whose function never names `is_terminal` is ungated, \
             and AC-7 is structurally blind to that: inverting the gate leaves \
             every piped `cli_e2e` test green (measured, requirement.md AC-7)"
        );

        // Non-vacuity: the verb this sweep counts still exists to be counted.
        let render = sources
            .iter()
            .find(|(rel, _)| rel == "render.rs")
            .map(|(_, src)| crate::status::scan::code_only(src))
            .expect("render.rs is a production source");
        assert!(
            render.contains("fn with_markdown("),
            "this assertion is only meaningful while `render.rs` owns the \
             renderer-attaching constructor"
        );
    }
}
