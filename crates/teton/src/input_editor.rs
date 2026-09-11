//! The line the user is typing during a turn, as a pure state machine
//! (REQ-622 ADR-622-2).
//!
//! Inside a turn the terminal leaves canonical mode (ADR-622-1), and that
//! retires the kernel's line discipline for the duration: nothing assembles
//! characters out of bytes any more, nothing implements Backspace, nothing
//! echoes, and nothing decides where a line ends. This module takes that job
//! over. It is handed whatever a `read(2)` returned and it answers with the line
//! so far, the row that should be drawn for it, and the lines the user has
//! already submitted — without touching a terminal, a clock, or a file
//! descriptor.
//!
//! Two properties are load-bearing and are why this is a module rather than a
//! few lines in the pump:
//!
//! 1. **No I/O, no terminal, and no bytes of its own.** [`InputEditor::push`] is
//!    a pure transition on the keystrokes seen and [`InputEditor::row`] a pure
//!    function of the line and a caller-supplied width. BR-1 makes the whole
//!    feature vanish when stdin or stdout is not a terminal, so every piped test
//!    is structurally blind to it, and a pty leg can only observe what a real
//!    terminal chose to draw. Decoding inside the read path would leave BR-3's
//!    core behaviour — *which bytes are one character, which are a keystroke,
//!    and which are dropped* — with no verification route but a terminal
//!    (BR-10, LESSON-481): the TTY gate would double as a test blindfold.
//! 2. **Nothing typed can leave here except through the queue.** There is no
//!    passthrough. [`InputEditor::take_next_queued`] is the only way out toward
//!    the wire, only Enter puts anything in the queue, and every byte outside
//!    the handled set is *dropped* rather than held or forwarded (BR-9) — Ctrl-C
//!    and Ctrl-D included, which is BR-15 by construction rather than by an arm
//!    that remembers to ignore them. An escape-prefixed sequence is consumed as
//!    a unit, so an arrow key cannot leave its final byte behind to be echoed as
//!    text.
//!
//! The third thing this module owns is the seam a mid-turn question crosses
//! (BR-5). [`InputEditor::shelve`] moves the pending line aside and leaves a
//! *fresh* buffer for the question to read, so an answer cannot be prefixed by
//! type-ahead and a line queued earlier in the turn cannot be eaten by a
//! question; [`InputEditor::unshelve`] puts the original line back verbatim.
//! One editor serves the pump and the prompter precisely so that seam exists
//! once (LESSON-502) rather than in each of two readers of stdin.
//!
//! # What breaks which test
//!
//! The mutation below was **applied and observed failing** (AC-10), not reasoned
//! about — a suite that stays green with the feature disabled has not tested the
//! feature (LESSON-441, LESSON-464, LESSON-569):
//!
//! | Mutation | Fails |
//! |---|---|
//! | Backspace pops a **byte** rather than a `char` | **1 red of 853** in this binary's suite and **1 red of 49** in `pty_e2e` (re-run 2026-09-10 over the finished suite). Unit: `the_keystroke_table`, and inside it **only the three multi-byte rows** — one Backspace over `é`, over a CJK ideograph, over an emoji — each leaving `U+FFFD` where the line should be empty; it fails on the `é` row first. Pty: `multi_byte_input_round_trips`, on the emoji, where three bytes of a four-byte character stay in the line |
//!
//! Every ASCII row stayed green, and that is the finding rather than a footnote:
//! a keystroke table written only in ASCII would have passed this mutation
//! whole, because ASCII is the one alphabet in which a byte and a character are
//! the same thing.
//!
//! **The re-run corrected this record's own claim about the pty legs.** Written
//! before TASK-419 landed, it predicted the terminal could not see this at all —
//! a terminal draws a replacement character as willingly as a letter. Half of
//! that is right and half was wrong: the emoji case leaves a *truncated* four-byte
//! sequence rather than a clean `U+FFFD`, which is an unprintable row and a
//! prompt that cannot be sent, so the AC-8 leg does catch it. The 48 other pty
//! legs stay green, which is the half that held — the ASCII ones cannot see it,
//! and the three-row granularity inside the table is still the unit's alone
//! (LESSON-481). A prediction re-run rather than left standing is why this
//! paragraph is a finding instead of a plausible sentence (LESSON-652).

use crate::markdown::display_width;
use crate::render::defused;

/// What the pending row is prefixed with.
///
/// Fixed, and deliberately not the entry frame's prompt: this row is drawn
/// *inside* a turn, under the activity row, and it has to read as "what you are
/// typing will be sent next" rather than as a prompt that is accepting input
/// now. Two columns wide, which is the whole of the row's overhead.
const MARKER: &str = "> ";

/// The two bytes a Backspace key arrives as.
///
/// `DEL` is what a terminal's erase character is on every platform this ships
/// to; `BS` is what Ctrl-H sends and what a handful of terminal emulators are
/// configured to send instead. Handling both is cheaper than being wrong about
/// which one a user's terminal chose.
const DEL: u8 = 0x7f;
const BS: u8 = 0x08;

/// Enter, in both of the forms it can arrive in.
///
/// `ICRNL` survives [`crate::prompt`]'s mode change (ADR-622-1), so a real Enter
/// arrives as `LF`. `CR` is here for pasted text, which carries whatever line
/// ending the clipboard held.
const LF: u8 = b'\n';
const CR: u8 = b'\r';

/// The bytes that open an escape sequence, and the two forms this decoder knows.
///
/// `ESC [` is CSI — arrow keys, Home/End, and every modified variant of them,
/// with any number of parameter bytes before a final byte. `ESC O` is SS3, which
/// is what an application-mode keypad and the function keys send, and which
/// always carries exactly one byte after the `O`.
const ESC: u8 = 0x1b;
const CSI: u8 = b'[';
const SS3: u8 = b'O';

/// The range a CSI sequence's **final** byte falls in.
///
/// Written down rather than inferred, because "the sequence ends here" is the
/// one decision that decides whether the rest of an arrow key is dropped or
/// echoed as text. Everything before the final byte is a parameter or an
/// intermediate byte and is dropped with it.
const FINAL_FIRST: u8 = 0x40;
const FINAL_LAST: u8 = 0x7e;

/// How many bytes of one escape sequence are **stored** before the rest are
/// dropped unstored.
///
/// A sequence has no length limit on the wire, and this decoder holds one across
/// pushes (see [`InputEditor::partial`]), so without a cap a stream that never
/// sends a final byte would grow the buffer without bound. Sixteen is several
/// times the longest sequence any terminal actually sends (`ESC [ 1 ; 5 A` is
/// six).
///
/// Past the cap the bytes are dropped but the sequence stays **open** until its
/// final byte arrives. Abandoning it instead would bound the memory and spill
/// the tail of the sequence into the user's line as text, which is the one
/// outcome BR-9 rules out — a cap that turned garbage into a prompt would be
/// worse than no cap at all.
const MAX_SEQUENCE_BYTES: usize = 16;

/// What one decoded keystroke changed, so the caller knows what to repaint.
///
/// Returned per keystroke rather than as a summary of the whole push, because
/// the two callers ask different questions of the same bytes: the pump wants to
/// know whether the pending row moved and whether the queue's count did, and
/// the prompter wants to know whether the line it is reading is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The pending line changed, so its row is stale.
    Pending,
    /// Enter was seen; the queue now holds this many lines.
    ///
    /// The count is always the queue's length *now*, which is what the activity
    /// row's `· N queued` clause needs. Where a question is open the submitted
    /// line is that question's answer and the queue is untouched (BR-5), so the
    /// count is unchanged — and the reader that cares is the prompter, which
    /// reads this variant as "the answer is complete" and takes it with
    /// [`InputEditor::take_answer`]. Only one of the two ever reads a push while
    /// a question is open (ADR-622-2), so the variant is unambiguous at the
    /// caller even though it is not at the enum.
    Queued(usize),
    /// A keystroke was consumed and nothing changed.
    ///
    /// Not the same as an empty return, and the difference is BR-9's whole
    /// claim: a dropped control byte or a consumed escape sequence is a decision
    /// this module made, and the caller is told so. Bytes that are merely *held*
    /// — half of a character, the middle of an escape sequence — decide nothing
    /// yet and produce no `Edit` at all.
    Nothing,
}

/// The line being typed, the lines already submitted, and the line a question
/// is standing on top of.
///
/// Every field is content; none of it is a terminal fact. The editor does not
/// know how wide the terminal is (that is [`Self::row`]'s argument), whether one
/// is attached, or whether raw mode is engaged.
#[derive(Debug, Default)]
pub struct InputEditor {
    /// What the user has typed and not yet submitted.
    ///
    /// A `String`, so it holds *characters*: everything above this line is byte
    /// decoding and everything below it — the row, Backspace, the queue — is
    /// character work. That boundary is where the [`Self::backspace`] mutation
    /// lands.
    pending: String,
    /// Lines submitted during the turn, oldest first.
    ///
    /// Drained one per entry-loop iteration by [`Self::take_next_queued`]
    /// (ADR-622-5), so each queued line becomes its own turn with its own
    /// hand-off and cost line rather than a batch that shares one.
    queued: Vec<String>,
    /// The pending line a question is standing on top of (BR-5).
    ///
    /// `Some` exactly while a question owns the terminal, which is also how
    /// [`Self::enter`] knows a submitted line is an answer rather than a queued
    /// prompt. `Some(String::new())` is a real state — a question opened with
    /// nothing typed — and it is why this is an `Option` rather than a
    /// non-empty-string convention.
    shelved: Option<String>,
    /// Bytes consumed whose keystroke is not yet decided.
    ///
    /// Three things share this buffer, discriminated by its first byte, and they
    /// are disjoint because neither `CR` (0x0d) nor `ESC` (0x1b) can appear
    /// anywhere in a multi-byte UTF-8 sequence — every byte of one is 0x80 or
    /// above:
    ///
    /// - `[CR]` — a `CR` that has already fired its Enter, held so that the `LF`
    ///   of a `CRLF` is swallowed as part of the same Enter rather than firing a
    ///   second one. It has to be held across pushes because a paste large
    ///   enough to be split by the kernel can be split between the two bytes.
    /// - `[ESC, …]` — an escape sequence being consumed (BR-9). Also held across
    ///   pushes, and capped at [`MAX_SEQUENCE_BYTES`].
    /// - `[lead, cont…]` — an incomplete UTF-8 character. A terminal delivers a
    ///   multi-byte character as whatever bytes were ready, so `é` really does
    ///   arrive as two pushes, and a decoder that dropped the tail would make
    ///   every non-ASCII keystroke unreliable rather than merely wrong.
    partial: Vec<u8>,
}

impl InputEditor {
    /// Decode `bytes` into keystrokes, returning what each one changed.
    ///
    /// One `Edit` per keystroke *decided*, in order, which is why this returns a
    /// `Vec` rather than a single value: a pasted block is several Enters, and
    /// a caller that saw only the last of them would queue one line and repaint
    /// a count for three (BR-6). Bytes that are held rather than decided
    /// contribute nothing — an empty return means "consumed, nothing to
    /// repaint", not "nothing arrived".
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Edit> {
        let mut edits = Vec::new();
        for &b in bytes {
            if let Some(edit) = self.byte(b) {
                edits.push(edit);
            }
        }
        edits
    }

    /// The pending line as one row, or `None` when there is nothing to draw.
    ///
    /// The **tail** that fits, not the head: the cursor rests at the end of what
    /// the user is typing, so a line longer than the terminal has to scroll with
    /// the cursor. That is the one respect in which this differs from
    /// [`crate::activity`]'s row, which shows the head of a sentence it composed
    /// itself, and it is why the fit here is its own function rather than that
    /// module's.
    ///
    /// Measured on the **defused** string, with the display width
    /// `markdown.rs` owns (ASSUME-023), and within `width - 1` for two reasons,
    /// the second of which is this row's alone.
    ///
    /// The first is [`crate::activity::TurnActivity::frame`]'s: a CJK character
    /// counted as one column but drawn as two makes the row exceed the
    /// terminal, which then hard-wraps it into a second row the withdraw does
    /// not clear.
    ///
    /// The second is the cursor, and it is a fact about this row and not about
    /// that one. This row is the row the cursor **is on** (REQ-622 ADR-622-4):
    /// it is drawn with [`crate::render::Surface::draw_current_row`], which
    /// writes no trailing newline, so the terminal's caret ends up immediately
    /// after the last character the user typed — where a caret belongs. A row
    /// filled to the very last column would put that caret one column past the
    /// window, and a terminal resolves that either by wrapping to a row this
    /// block does not own or by parking the caret on top of the last character.
    /// The reserved column is where the caret sits instead. (The activity row
    /// above spends no such column and needs none: the cursor has stepped past
    /// it.)
    ///
    /// The defuse is load-bearing even though [`Self::push`] drops every control
    /// byte: a bidi override is *printable* UTF-8, so `U+202E` typed or pasted
    /// into this line is assembled into [`Self::pending`] exactly as any other
    /// character, and it is the row — the thing a terminal reads — that has to
    /// neutralize it. Sanitizing at the writer rather than at the store keeps
    /// what the user typed recoverable (LESSON-474): the queued prompt carries
    /// the character, and only its rendering loses it.
    ///
    /// **`queued_hint`** is BR-14's count, for the moments the activity row is
    /// not there to carry it (REQ-622, verify). `TurnActivity::frame` answers
    /// `None` for the whole of `Streaming` — a reply arriving is its own
    /// feedback (ADR-621-1) — so an Enter pressed while the model was writing
    /// took the pending row down and said *nothing at all*, which is
    /// indistinguishable from a swallowed keystroke. `Some(n)` with `n` nonzero
    /// puts `[n queued] ` ahead of the marker; `Some(0)` and `None` both leave
    /// the marker alone, and `None` specifically means "the activity row is
    /// carrying this count", so the two can never both say it. The caller that
    /// decides which is the pump, which is the only thing that knows whether the
    /// activity row is due.
    ///
    /// A hint is also enough on its own to make a row: a queued line usually
    /// *empties* the pending buffer, so `[1 queued] > ` with nothing after it is
    /// exactly the state the user needs to see.
    ///
    /// `None` covers two cases, and both mean the same thing to the caller
    /// ("draw no pending row"): nothing to say — an empty line and no count to
    /// report — and a terminal with no room for the marker and a column of text.
    #[must_use]
    pub fn row(&self, width: usize, queued_hint: Option<usize>) -> Option<String> {
        let waiting = queued_hint.filter(|n| *n != 0);
        if self.pending.is_empty() && waiting.is_none() {
            return None;
        }
        let marker = match waiting {
            Some(n) => format!("[{n} queued] {MARKER}"),
            None => MARKER.to_owned(),
        };
        let budget = width.saturating_sub(1);
        let marker_width = display_width(&marker);
        if budget <= marker_width {
            return None;
        }
        let mut row = marker;
        row.push_str(&tail_that_fits(
            &defused(&self.pending),
            budget - marker_width,
        ));
        Some(row)
    }

    /// Move the pending line aside and leave a fresh buffer for a question
    /// (BR-5).
    ///
    /// Called where a question is *drawn*, so that everything the question reads
    /// was typed after the user could see it. The partial-character half of
    /// [`Self::partial`] is discarded at the same moment and for the same
    /// reason: a byte typed before the question is not part of the answer. An
    /// escape sequence or a held `CR` is *kept* across the seam, which looks
    /// like the opposite rule and is the same one — those bytes are mid-drop,
    /// and forgetting them would let an arrow key's final byte land in the
    /// answer as text.
    ///
    /// A second `shelve` with one already outstanding keeps the first. Nested
    /// questions are not reachable (a question inside a turn reuses that turn's
    /// raw mode, ADR-622-3), and if they became reachable the line that must
    /// survive is the user's own rather than an inner question's draft.
    pub fn shelve(&mut self) {
        if self.shelved.is_none() {
            self.shelved = Some(std::mem::take(&mut self.pending));
        }
        self.forget_partial_char();
    }

    /// Put the shelved line back verbatim, discarding whatever the question left
    /// behind (BR-5).
    ///
    /// Verbatim is the promise: the user's half-written thought reappears
    /// exactly as it was, because a line that came back subtly different would
    /// be worse than one that came back empty. Whatever is in the fresh buffer
    /// is dropped rather than merged — the caller has already taken the answer
    /// it wanted with [`Self::take_answer`], and anything still there is the
    /// tail of an answer nobody asked for.
    ///
    /// A no-op with nothing shelved, which is the honest reading of "restore
    /// what was put aside" when nothing was.
    pub fn unshelve(&mut self) {
        if let Some(text) = self.shelved.take() {
            self.pending = text;
            self.forget_partial_char();
        }
    }

    /// Take the line the question was reading, leaving its buffer empty.
    ///
    /// A `String` rather than an `Option<String>`: an empty answer is a real
    /// answer — a bare Enter at a question that offers a default — and a caller
    /// that had to distinguish "empty" from "nothing" would be asking a question
    /// this editor cannot answer, since it does not know whether the prompt is
    /// still open.
    pub fn take_answer(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    /// The line in the buffer as it stands, for the caller that has to draw it.
    ///
    /// The read-only half of [`Self::take_answer`], and the prompter's echo
    /// source (ADR-622-2): the row a question's answer is painted into is
    /// composed from *this* text and not from the bytes the reader happened to
    /// see, so the assembling — which bytes are one character, which byte was a
    /// Backspace, which control byte was dropped — keeps exactly one
    /// implementation (BR-2). A writer that echoed its own input would be a
    /// second decoder, and the one that drifted would paint half a character.
    ///
    /// Deliberately **not** [`Self::row`], which composes the *pump's* pending
    /// row — marker, defuse and width fit included. A question's answer row is
    /// the prompter's: it is prefixed by the question rather than by a marker,
    /// and it is defused at that writer for the same reason this one defuses at
    /// its own.
    #[must_use]
    pub fn answer_so_far(&self) -> &str {
        &self.pending
    }

    /// Take the oldest queued line, or `None` when the queue is empty.
    ///
    /// Oldest first, one per call: the entry loop drains a single line per
    /// iteration (ADR-622-5) so each queued prompt takes the whole typed-line
    /// path — `slash::classify`, the REQ-615 `cd` intercept, every pre-send
    /// check — with its own turn around it. `remove(0)` rather than a
    /// `VecDeque`, because order is the contract and the queue holds the handful
    /// of lines a person can type while one turn runs.
    pub fn take_next_queued(&mut self) -> Option<String> {
        if self.queued.is_empty() {
            None
        } else {
            Some(self.queued.remove(0))
        }
    }

    /// How many lines are waiting to be sent, for the activity row's clause
    /// (BR-14).
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queued.len()
    }

    /// Decode one byte, in whatever state the last one left behind.
    fn byte(&mut self, b: u8) -> Option<Edit> {
        match self.partial.first().copied() {
            Some(CR) => {
                self.partial.clear();
                if b == LF {
                    // The `LF` of one `CRLF`: the `CR` already fired the Enter.
                    return None;
                }
                self.fresh(b)
            }
            Some(ESC) => self.escape(b),
            Some(_) => self.continuation(b),
            None => self.fresh(b),
        }
    }

    /// Decode one byte with nothing held.
    ///
    /// Guards rather than byte ranges throughout, so no two arms can come to
    /// overlap as the set grows — and so the fall-through at the bottom stays
    /// what BR-9 asks for: anything this decoder does not recognise is dropped,
    /// never echoed and never forwarded.
    fn fresh(&mut self, b: u8) -> Option<Edit> {
        match b {
            DEL | BS => Some(self.backspace()),
            LF => Some(self.enter()),
            CR => {
                let edit = self.enter();
                // Held so a `CRLF` is one Enter and not two.
                self.partial.push(CR);
                Some(edit)
            }
            ESC => {
                self.partial.push(ESC);
                None
            }
            // 0x00..=0x1f and 0x7f, less the four handled above: Ctrl-C, Ctrl-D
            // and every other bare control byte (BR-9, BR-15). Tab is here too —
            // completion is not this REQ's, and a tab echoed into the line would
            // be a column count the row cannot predict.
            b if b.is_ascii_control() => Some(Edit::Nothing),
            b if b == b' ' || b.is_ascii_graphic() => {
                self.pending.push(char::from(b));
                Some(Edit::Pending)
            }
            b => {
                if utf8_len(b).is_some() {
                    self.partial.push(b);
                    None
                } else {
                    // A stray continuation byte or an invalid lead: there is no
                    // character here to wait for.
                    Some(Edit::Nothing)
                }
            }
        }
    }

    /// Decode one byte with part of a UTF-8 character held.
    fn continuation(&mut self, b: u8) -> Option<Edit> {
        if !is_continuation(b) {
            // The character was truncated by a byte that cannot continue it —
            // a keystroke arriving after a dropped-frame's worth of garbage, or
            // a paste of invalid bytes. Drop what is held and read `b` as the
            // start of something new rather than losing it too.
            self.partial.clear();
            return self.fresh(b);
        }
        self.partial.push(b);
        let Some(want) = utf8_len(self.partial[0]) else {
            // Unreachable: `fresh` holds a byte only when `utf8_len` accepted
            // it. Dropped rather than asserted — the invariant belongs to this
            // file, and a panic in the input path would be a worse answer than
            // a lost keystroke.
            self.partial.clear();
            return Some(Edit::Nothing);
        };
        if self.partial.len() < want {
            return None;
        }
        let held = std::mem::take(&mut self.partial);
        match std::str::from_utf8(&held) {
            Ok(text) => {
                self.pending.push_str(text);
                Some(Edit::Pending)
            }
            // Complete in length and still not a character: an overlong
            // encoding, a surrogate, or a code point above U+10FFFF. Dropped,
            // because the alternative is `U+FFFD` in a line the user will send.
            Err(_) => Some(Edit::Nothing),
        }
    }

    /// Consume one byte of an escape sequence (BR-9).
    fn escape(&mut self, b: u8) -> Option<Edit> {
        if self.partial.len() == 1 {
            if b == CSI || b == SS3 {
                self.partial.push(b);
                return None;
            }
            // `ESC` followed by anything else — Alt-x, or a bare Escape then a
            // keystroke. Both bytes go: the byte after `ESC` is part of the
            // sequence, and echoing it would put a character in the line that
            // the user pressed a modifier to avoid typing.
            self.partial.clear();
            return Some(Edit::Nothing);
        }
        if self.partial[1] == SS3 {
            // `ESC O x`: exactly one byte, whatever it is.
            self.partial.clear();
            return Some(Edit::Nothing);
        }
        if (FINAL_FIRST..=FINAL_LAST).contains(&b) {
            self.partial.clear();
            return Some(Edit::Nothing);
        }
        // A parameter or intermediate byte: dropped with the sequence, and
        // stored only up to the cap (see [`MAX_SEQUENCE_BYTES`]).
        if self.partial.len() < MAX_SEQUENCE_BYTES {
            self.partial.push(b);
        }
        None
    }

    /// Erase the last **character** of the pending line.
    ///
    /// A character, not a byte, and that is the whole of this function.
    /// `String::pop` is what makes it so.
    ///
    /// **Mutation (AC-10), applied and observed red:** popping a byte instead —
    /// `String::from_utf8_lossy(&bytes[..bytes.len() - 1])` over
    /// `self.pending.as_bytes()` — cuts a multi-byte character apart, so one
    /// Backspace over `é` yields a replacement character rather than an empty
    /// line. Re-run 2026-09-10 over the finished suite: **1 red of 853** in this
    /// binary (`the_keystroke_table`, on its `é`, CJK and emoji rows and none of
    /// its ASCII ones) and **1 red of 49** in `pty_e2e`
    /// (`multi_byte_input_round_trips`, on the emoji). Reverted with the same
    /// edit. The module's table has the reading, including the prediction the
    /// re-run corrected.
    fn backspace(&mut self) -> Edit {
        if self.pending.pop().is_some() {
            Edit::Pending
        } else {
            Edit::Nothing
        }
    }

    /// Submit the pending line: a queued prompt, or a question's answer.
    fn enter(&mut self) -> Edit {
        if self.shelved.is_some() {
            // A question owns the terminal, so this line is its answer and the
            // queue must not see it (BR-5). It stays in the buffer for
            // `take_answer`, which is the caller that knows a question is open.
            return Edit::Queued(self.queued.len());
        }
        if self.pending.is_empty() {
            // An empty line is not a prompt. Queuing one would send an empty
            // turn after the current one ends, which no keystroke asked for.
            return Edit::Nothing;
        }
        let line = std::mem::take(&mut self.pending);
        self.queued.push(line);
        Edit::Queued(self.queued.len())
    }

    /// Drop a half-typed character, keeping a mid-drop escape sequence or a
    /// held `CR`.
    ///
    /// The asymmetry is [`Self::shelve`]'s paragraph: bytes that would become
    /// *text* must not cross the seam, and bytes that are being *dropped* must
    /// finish being dropped on the far side of it. Both are BR-5.
    fn forget_partial_char(&mut self) {
        if self.partial.first().is_some_and(|b| *b >= 0x80) {
            self.partial.clear();
        }
    }
}

/// How many bytes the UTF-8 character starting with `lead` occupies, or `None`
/// where `lead` cannot start one.
///
/// `0xc0` and `0xc1` are accepted as two-byte leads even though no valid
/// character starts with them: holding two bytes and letting
/// [`std::str::from_utf8`] reject the pair is one rule where excluding them here
/// would be two, and the outcome — the bytes are dropped — is identical.
fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

/// Whether `b` can continue a UTF-8 character.
fn is_continuation(b: u8) -> bool {
    (0x80..=0xbf).contains(&b)
}

/// The longest **tail** of `shown` that fits in `budget` display columns.
///
/// Measured as a **string** after every candidate, never as a running sum of
/// per-character widths: `unicode-width` charges an emoji presentation sequence
/// (a base plus `U+FE0F`) two columns as a string and one as two characters, so
/// a per-character sum admits twice the row the terminal will draw — the same
/// wrapped row, and the same residue, that [`InputEditor::row`]'s defuse closes
/// for control bytes. The loop stops at the first candidate that does not fit,
/// so it runs `budget + 1` times at worst rather than once per character of a
/// long line.
///
/// Called on already-defused text and on nothing else, for
/// [`crate::activity`]'s reason: the two measurements a fit makes disagree about
/// a control character, and defusing at the caller makes that disagreement
/// unreachable rather than merely unlikely.
fn tail_that_fits(shown: &str, budget: usize) -> String {
    if display_width(shown) <= budget {
        return shown.to_owned();
    }
    let mut tail = String::new();
    for c in shown.chars().rev() {
        let mut candidate = String::with_capacity(tail.len() + c.len_utf8());
        candidate.push(c);
        candidate.push_str(&tail);
        if display_width(&candidate) > budget {
            break;
        }
        tail = candidate;
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of [`a_queued_hint_prefixes_the_marker_and_can_make_a_row_by_itself`]:
    /// what the case is, the bytes that make it, the terminal's width, the
    /// hint the pump would pass, and the literal row.
    ///
    /// A named tuple for [`Case`]'s reason, and because the hint is an
    /// `Option<usize>` whose two `None`-ish values mean different things — the
    /// alias is where "which position is the hint" is written down once.
    type HintCase<'a> = (&'a str, &'a [u8], usize, Option<usize>, Option<&'a str>);

    /// One row of [`the_keystroke_table`]: what the case is, the pushes that
    /// make it, and the literal state and row the editor must then show at that
    /// width.
    type Case<'a> = (
        &'a str,
        &'a [&'a [u8]],
        &'a str,
        &'a [&'a str],
        usize,
        Option<&'a str>,
    );

    /// Feed every push in order and hand back the editor that resulted.
    fn drive(pushes: &[&[u8]]) -> InputEditor {
        let mut editor = InputEditor::default();
        for push in pushes {
            editor.push(push);
        }
        editor
    }

    /// The AC-10 oracle: byte sequences to **literal** `(pending, queued, row)`
    /// triples.
    ///
    /// Every expectation below is written down rather than computed. That is
    /// LESSON-569's rule and it is the whole reason this table exists in this
    /// shape: an oracle that asked the editor what it thought the row was would
    /// pass under the [`InputEditor::backspace`] mutation, under a fit that
    /// measured characters, and under a decoder that echoed an arrow key —
    /// which is to say it would test nothing. The inputs are readable
    /// (`"中".as_bytes()`) because the *input* is not the oracle; the split of
    /// `é` is spelled by index because where the split falls is the point.
    ///
    /// The row invariant travels with the table rather than in a test of its
    /// own: a row wider than `width - 1` is hard-wrapped by the terminal into a
    /// second row the withdraw does not clear, so it has to hold for every case
    /// here and not only for the two that were written to stress it.
    #[test]
    fn the_keystroke_table() {
        let cases: &[Case] = &[
            ("ASCII", &[b"hi"], "hi", &[], 20, Some("> hi")),
            (
                "é split across two pushes",
                &[&"\u{e9}".as_bytes()[..1], &"\u{e9}".as_bytes()[1..]],
                "\u{e9}",
                &[],
                20,
                Some("> \u{e9}"),
            ),
            (
                "a CJK pair",
                &["中文".as_bytes()],
                "中文",
                &[],
                20,
                Some("> 中文"),
            ),
            ("an emoji", &["🙂".as_bytes()], "🙂", &[], 20, Some("> 🙂")),
            (
                "Backspace over ASCII",
                &[b"hi\x7f"],
                "h",
                &[],
                20,
                Some("> h"),
            ),
            ("Ctrl-H over ASCII", &[b"hi\x08"], "h", &[], 20, Some("> h")),
            (
                "Backspace over é",
                &["\u{e9}".as_bytes(), b"\x7f"],
                "",
                &[],
                20,
                None,
            ),
            (
                "Backspace over a CJK character",
                &["中".as_bytes(), b"\x7f"],
                "",
                &[],
                20,
                None,
            ),
            (
                "Backspace over an emoji",
                &["🙂".as_bytes(), b"\x7f"],
                "",
                &[],
                20,
                None,
            ),
            ("Backspace on an empty line", &[b"\x7f"], "", &[], 20, None),
            ("Enter", &[b"hi\n"], "", &["hi"], 20, None),
            (
                "CRLF is one Enter",
                &[b"a\r\nb"],
                "b",
                &["a"],
                20,
                Some("> b"),
            ),
            (
                "a bare CR is Enter",
                &[b"a\rb"],
                "b",
                &["a"],
                20,
                Some("> b"),
            ),
            (
                "CRLF split across two pushes",
                &[b"a\r", b"\nb"],
                "b",
                &["a"],
                20,
                Some("> b"),
            ),
            (
                "three lines in one push",
                &[b"one\ntwo\nthree\n"],
                "",
                &["one", "two", "three"],
                20,
                None,
            ),
            (
                "a block whose last line has no newline",
                &[b"one\ntwo"],
                "two",
                &["one"],
                20,
                Some("> two"),
            ),
            ("an up arrow", &[b"a\x1b[Ab"], "ab", &[], 20, Some("> ab")),
            (
                "a function key",
                &[b"a\x1bOPb"],
                "ab",
                &[],
                20,
                Some("> ab"),
            ),
            ("Ctrl-D", &[b"a\x04b"], "ab", &[], 20, Some("> ab")),
            ("Ctrl-C", &[b"a\x03b"], "ab", &[], 20, Some("> ab")),
            (
                "a bidi override stays in the line and not in the row",
                &["a\u{202e}b".as_bytes()],
                "a\u{202e}b",
                &[],
                20,
                Some("> a b"),
            ),
            (
                "an ASCII tail at a narrow width",
                &[b"abcdefghij"],
                "abcdefghij",
                &[],
                10,
                Some("> defghij"),
            ),
            (
                "a CJK tail at a narrow width",
                &["中文中文中".as_bytes()],
                "中文中文中",
                &[],
                10,
                Some("> 中文中"),
            ),
            (
                "a CJK glyph with no room leaves the marker",
                &["中".as_bytes()],
                "中",
                &[],
                4,
                Some("> "),
            ),
            (
                "no room for the marker and a column",
                &[b"hi"],
                "hi",
                &[],
                3,
                None,
            ),
        ];

        for (what, pushes, pending, queued, width, row) in cases {
            let editor = drive(pushes);
            assert_eq!(editor.pending, *pending, "{what}: pending");
            let want: Vec<String> = queued.iter().map(|line| (*line).to_owned()).collect();
            assert_eq!(editor.queued, want, "{what}: queued");
            let drawn = editor.row(*width, None);
            assert_eq!(drawn.as_deref(), *row, "{what}: row");
            if let Some(drawn) = drawn {
                assert!(
                    display_width(&drawn) <= width.saturating_sub(1),
                    "{what}: {} columns at a width of {width}: {drawn:?}",
                    display_width(&drawn)
                );
            }
        }
    }

    /// BR-5: a question reads only what was typed after it was drawn, and the
    /// line it interrupted comes back verbatim.
    ///
    /// Three ways the seam can leak are asserted here rather than assumed: the
    /// answer must not carry type-ahead, it must not carry the second half of a
    /// character typed before the question, and it must not carry the tail of an
    /// escape sequence that was mid-drop when the question opened. The queue is
    /// checked from both sides — the answer never enters it, and a line already
    /// in it is not consumed by the question.
    #[test]
    fn a_shelved_line_is_untouched_by_an_answer() {
        let mut editor = InputEditor::default();
        editor.push(b"first\n");
        editor.push(b"second half");
        editor.push(&"\u{e9}".as_bytes()[..1]);
        assert_eq!(editor.queued_len(), 1);

        editor.shelve();
        assert_eq!(editor.pending, "", "the question reads a fresh buffer");
        assert_eq!(editor.row(20, None), None);

        // The tail of that half-typed character arrives after the question was
        // drawn. It belongs to the line before the seam, so it neither
        // completes into the answer nor appears in it.
        assert_eq!(editor.push(&"\u{e9}".as_bytes()[1..]), vec![Edit::Nothing]);
        assert_eq!(
            editor.push(b"yes\n"),
            vec![Edit::Pending, Edit::Pending, Edit::Pending, Edit::Queued(1)]
        );
        assert_eq!(editor.take_answer(), "yes");
        assert_eq!(editor.queued_len(), 1, "the answer never entered the queue");

        editor.unshelve();
        assert_eq!(editor.pending, "second half", "restored verbatim");
        assert_eq!(editor.row(20, None).as_deref(), Some("> second half"));
        assert_eq!(
            editor.take_next_queued().as_deref(),
            Some("first"),
            "the queued line survived the question"
        );
        assert_eq!(editor.take_next_queued(), None);

        // An escape sequence straddling the seam finishes being dropped on the
        // far side of it: the arrow key's final byte is not the answer's first
        // character.
        let mut straddled = InputEditor::default();
        straddled.push(b"a thought");
        straddled.push(b"\x1b[");
        straddled.shelve();
        assert_eq!(
            straddled.push(b"Ay\n"),
            vec![Edit::Nothing, Edit::Pending, Edit::Queued(0)]
        );
        assert_eq!(straddled.take_answer(), "y");
        straddled.unshelve();
        assert_eq!(straddled.pending, "a thought");

        // A second shelve keeps the first: the line that has to survive an
        // unreachable nested question is the user's own.
        let mut twice = InputEditor::default();
        twice.push(b"outer");
        twice.shelve();
        twice.push(b"inner");
        twice.shelve();
        assert_eq!(twice.pending, "inner");
        twice.unshelve();
        assert_eq!(twice.pending, "outer");
    }

    /// BR-6: Enter queues in order, and a pasted block queues one prompt per
    /// line.
    #[test]
    fn enter_queues_and_a_pasted_block_queues_one_per_line() {
        let mut editor = InputEditor::default();
        let edits = editor.push(b"one\ntwo\nthree\n");
        assert_eq!(
            edits
                .iter()
                .filter(|edit| matches!(edit, Edit::Queued(_)))
                .copied()
                .collect::<Vec<_>>(),
            vec![Edit::Queued(1), Edit::Queued(2), Edit::Queued(3)],
            "the count on each Enter, so the row's clause cannot lag the queue"
        );
        assert_eq!(editor.queued_len(), 3);
        assert_eq!(editor.pending, "");
        assert_eq!(editor.row(20, None), None);
        assert_eq!(editor.take_next_queued().as_deref(), Some("one"));
        assert_eq!(editor.take_next_queued().as_deref(), Some("two"));
        assert_eq!(editor.take_next_queued().as_deref(), Some("three"));
        assert_eq!(editor.take_next_queued(), None);
        assert_eq!(editor.queued_len(), 0);

        // An empty line is not a prompt.
        let mut empty = InputEditor::default();
        assert_eq!(empty.push(b"\n\n\n"), vec![Edit::Nothing; 3]);
        assert_eq!(empty.queued_len(), 0);

        // A CRLF block that the kernel split between the two bytes of one line
        // ending: still one prompt per line, not two.
        let mut pasted = InputEditor::default();
        pasted.push(b"alpha\r");
        pasted.push(b"\nbeta\r");
        pasted.push(b"\n");
        assert_eq!(pasted.queued, ["alpha", "beta"]);
        assert_eq!(pasted.pending, "");

        // The last line of a block with no trailing newline stays pending, and
        // one more Enter queues it.
        let mut trailing = InputEditor::default();
        trailing.push(b"alpha\nbeta");
        assert_eq!(trailing.queued_len(), 1);
        assert_eq!(trailing.pending, "beta");
        assert_eq!(trailing.push(b"\n"), vec![Edit::Queued(2)]);
        assert_eq!(trailing.queued, ["alpha", "beta"]);
    }

    /// **REQ-622 BR-14, verify: the queued count on the row that is on screen.**
    ///
    /// `TurnActivity::frame` answers `None` for the whole of `Streaming`
    /// (ADR-621-1), so for most of a long answer there is no activity row to
    /// carry the `· N queued` clause — and an Enter pressed there emptied the
    /// pending line, took its row down, and said nothing at all. The hint puts
    /// the count in front of the marker for exactly those moments.
    ///
    /// A table of literal rows rather than a predicate, so the *bytes* are the
    /// oracle: the marker's shape, the space before it, the fit, and the two
    /// ways of saying "the activity row has this" (`None`) and "there is nothing
    /// to say" (`Some(0)`), which must be indistinguishable at the row.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** drop the
    /// `&& waiting.is_none()` from the early return, so a hint no longer makes a
    /// row on its own. **2 red of 872** — this test, on the `[1 queued] > ` case
    /// with an empty buffer, and
    /// `client::tests::a_line_queued_while_the_reply_streams_is_still_reported`,
    /// which is the same case arriving through the pump. That case is the
    /// *common* one, not a corner: Enter is what empties the buffer, so the
    /// moment there is a count to report is the moment there is nothing else on
    /// the row. Reverted with the same edit.
    #[test]
    fn a_queued_hint_prefixes_the_marker_and_can_make_a_row_by_itself() {
        // (what, pushes, width, hint, row)
        let cases: &[HintCase<'_>] = &[
            (
                "an Enter with the activity row gone: the count is all there is \
                 to show, and it is shown",
                b"ship it\n",
                40,
                Some(1),
                Some("[1 queued] > "),
            ),
            (
                "a second line typed after it, with the first still waiting",
                b"ship it\nand then",
                40,
                Some(1),
                Some("[1 queued] > and then"),
            ),
            (
                "two waiting",
                b"one\ntwo\n",
                40,
                Some(2),
                Some("[2 queued] > "),
            ),
            (
                "the activity row is due, so it carries the count and the marker \
                 stays plain",
                b"ship it\nand then",
                40,
                None,
                Some("> and then"),
            ),
            (
                "nothing waiting reads exactly like nothing to hint",
                b"and then",
                40,
                Some(0),
                Some("> and then"),
            ),
            (
                "and with nothing waiting and nothing typed there is still no row",
                b"",
                40,
                Some(0),
                None,
            ),
            (
                "the prefix is part of the fit: the tail that fits is measured \
                 against it, not against the bare marker",
                b"one\n0123456789abcdefghij",
                24,
                Some(1),
                Some("[1 queued] > abcdefghij"),
            ),
            (
                "a terminal with no room for the prefix and a column of text \
                 draws nothing rather than a wrapped row",
                b"one\nx",
                12,
                Some(1),
                None,
            ),
        ];

        for (what, pushes, width, hint, expected) in cases {
            let mut editor = InputEditor::default();
            editor.push(pushes);
            assert_eq!(
                editor.row(*width, *hint).as_deref(),
                *expected,
                "{what} (width {width}, hint {hint:?})"
            );
        }
    }

    /// BR-9 and BR-15: an escape-prefixed sequence is consumed as a unit and a
    /// lone control byte is dropped — neither echoed, neither forwarded.
    ///
    /// Ctrl-D is in the table for BR-15 specifically: it submits nothing (the
    /// queue stays empty) and it ends nothing (there is no state here it can
    /// close). Ctrl-C is beside it because `ISIG` survives the mode change
    /// (ADR-622-1), so the byte only ever reaches this decoder on a terminal
    /// that declined to make it a signal — and it is dropped there too.
    #[test]
    fn unhandled_keys_change_nothing() {
        let unhandled: &[(&str, &[&[u8]])] = &[
            ("an up arrow", &[b"\x1b[A"]),
            ("a down arrow", &[b"\x1b[B"]),
            ("a modified left arrow", &[b"\x1b[1;5D"]),
            ("Home", &[b"\x1b[H"]),
            ("a function key", &[b"\x1bOP"]),
            (
                "a sequence split across three pushes",
                &[b"\x1b", b"[", b"A"],
            ),
            ("a bare Escape then a keystroke", &[b"\x1bx"]),
            ("Ctrl-C", &[b"\x03"]),
            ("Ctrl-D", &[b"\x04"]),
            ("Ctrl-A", &[b"\x01"]),
            ("a tab", &[b"\t"]),
            ("a stray continuation byte", &[b"\x80"]),
            ("an invalid lead byte", &[b"\xff"]),
            (
                "a sequence longer than the decoder holds",
                &[b"\x1b[", b"1;1;1;1;1;1;1;1;1;1;1;1", b"m"],
            ),
        ];

        for (what, pushes) in unhandled {
            let mut editor = InputEditor::default();
            editor.push(b"keep");
            let mut edits = Vec::new();
            for push in *pushes {
                edits.extend(editor.push(push));
            }
            assert!(
                edits.iter().all(|edit| *edit == Edit::Nothing),
                "{what}: {edits:?}"
            );
            assert_eq!(editor.pending, "keep", "{what}: pending");
            assert_eq!(editor.queued_len(), 0, "{what}: queued");
            assert_eq!(
                editor.row(20, None).as_deref(),
                Some("> keep"),
                "{what}: row"
            );
            assert_eq!(editor.take_next_queued(), None, "{what}: nothing to send");
        }
    }
}
