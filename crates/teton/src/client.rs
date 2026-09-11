//! The daemon connection: framing, handshake, autostart, and the event pump.
//!
//! Transport is intentionally thin — a synchronous, newline-delimited JSON-RPC
//! client matching the daemon's framing (`tetond::server`): one background thread
//! reads lines off the [`UnixStream`] and classifies each into an [`Incoming`]
//! (a response, a broadcast event, or a lag notice) on an [`mpsc`] channel; the
//! main thread writes requests and drains the channel. The CLI holds no HTTP
//! client of its own — every remote call is the daemon's job through its single
//! egress choke point (BR-1). All rendering happens through the [`Surface`] and
//! [`Prompter`] seams carried in [`UiContext`], so the pump is testable in the
//! rendering modules with scripted event streams, while this module is the small
//! untested socket shell.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use serde_json::Value;

use teton_protocol::events::{EVENT_METHOD, SUBSCRIPTION_LAGGED_METHOD};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Response, RpcError};
use teton_protocol::methods::{self, RpcMethod};
use teton_protocol::{ClientKind, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

use crate::firstrun;
use crate::input_editor::Edit;
use crate::model_ui;
use crate::prompt::{Prompter, RawMode, RawOutcome};
use crate::render::{LineKind, Surface};
use crate::session_ui::{self, EventOutcome, SessionState};
use teton_protocol::socket_path::DaemonPaths;

/// Diagnostic client name sent in the handshake.
const CLIENT_NAME: &str = "teton-cli";
/// This build's version, advertised in the handshake.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// How many times autostart polls for the socket before giving up.
const POLL_ATTEMPTS: usize = 50;
/// Delay between autostart connection attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One animation frame, and the longest either animating loop waits before it
/// looks at the world again (REQ-556, REQ-621 ADR-621-1).
///
/// Short enough that a lifecycle line lands promptly and an animation reads as
/// motion; long enough that an idle session is not spinning. It is the frame
/// interval *and* the wait timeout in both loops that animate — `main.rs`'s
/// entry loop polls stdin with it, and [`Connection::pump_until_answered`]
/// waits on this channel with it — so there is one clock, no timer thread and
/// no `sleep` anywhere in either.
///
/// It moved here from `main.rs` when the pump learned to wake: the entry
/// frame's dots and the turn's activity row are two animations of one session,
/// and a second constant is how they come to disagree about what a frame is.
/// The value is unchanged.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(120);

/// The rendering + input context threaded through the event pump.
pub struct UiContext<'a> {
    /// Where rendered output goes.
    pub surface: &'a mut dyn Surface,
    /// Running session state (tool titles, grants, cost meter).
    pub state: &'a mut SessionState,
    /// Interactive input source for permission prompts.
    pub prompter: &'a mut dyn Prompter,
    /// Whether this command owns an interactive session and should answer
    /// permission requests. Non-interactive commands (`doctor`, `cost`, config)
    /// only render them — the daemon broadcasts a permission request to every
    /// attached client, but only the owning interactive session should reply.
    pub answer_permissions: bool,
    /// Whether this command should answer a local-model proposal (REQ-547 BR-1).
    /// Same rule as `answer_permissions`, and for the same reason: the proposal
    /// is broadcast to every attached client, but only an interactive session
    /// asks the user. Other commands render it and leave it alone.
    pub answer_model_proposals: bool,
    /// Explicit opt-in auto-accept (BR-5 / AC-5): answer a proposal with `accept`
    /// and read no user input at all. Off unless `--yes` was passed.
    pub auto_accept_model: bool,
    /// Whether this process's **stdin** is a terminal — the one world-fact the
    /// `/model set` gate turns on (REQ-555 spec Permissions).
    ///
    /// It rides here rather than being read at handler depth because the slash
    /// handlers touch the world only through this context and the [`Surface`]
    /// seam — that is what lets a ratatui front-end inherit the commands by
    /// implementing the same seams (REQ-555 BR-9). A handler calling
    /// `std::io::stdin()` itself would be a second, invisible seam, and would
    /// leave one session holding two notions of "interactive" — this one and
    /// the stdout-derived flag that governs the banner and the entry frame.
    ///
    /// Stdin, not stdout, and read once at the edge: the stdout check asks
    /// whether *output* is a terminal, which says nothing about who produced the
    /// line that reached a handler.
    pub typed_input: bool,
    /// The session a slash command acts on, once one exists (REQ-563).
    ///
    /// `None` until `session/create` answers, and on every passive context — a
    /// `teton cost` in another terminal owns no session, and a command that needs
    /// one must say so rather than borrow whichever session it can see. `/web
    /// allow` lifts *a session's* taint restriction, so the id is an argument it
    /// cannot fabricate.
    ///
    /// It rides here for the same reason `typed_input` does: handlers touch the
    /// world only through this context, and a handler reaching for an ambient
    /// session id would be a second, invisible seam.
    pub session_id: Option<teton_protocol::SessionId>,
    /// What this session's `/name` lines may dispatch and what `/help` lists —
    /// the daemon's `skills/list` answer, held as a snapshot (REQ-585 BR-3,
    /// ADR-1/ADR-2).
    ///
    /// It rides here for `typed_input`'s and `session_id`'s reason: the slash
    /// handlers touch the world only through this context, and `/help` reading
    /// a registry from anywhere else would be a second, invisible seam. The
    /// snapshot is the client's *whole* knowledge of the user's `~/.claude` —
    /// there is no second reader of those directories in this crate.
    ///
    /// **Empty is the load-bearing default**, not a placeholder: it is the state
    /// of a user with no `~/.claude`, of every passive context, and of a client
    /// talking to a daemon that answers `skills/list` with `METHOD_NOT_FOUND`
    /// (ADR-2). An empty snapshot makes [`crate::slash::classify`] incapable of
    /// returning `Input::Skill`, so no `PromptTurnParams.skill` is ever sent and
    /// no skill consent can ever arrive — which is what makes "byte-for-byte
    /// what it is today" true for all three.
    ///
    /// Refreshed by [`Connection::refresh_skills`] after `session/create` and
    /// again after every `session_root_changed`
    /// ([`SessionState::take_skills_stale`]): a `/cd` re-derives the project
    /// half, and a name that outlived its snapshot would dispatch a skill the
    /// session no longer has.
    pub skills: crate::slash::SkillSnapshot,
}

impl UiContext<'_> {
    /// The cause pinning this session to the local tier, or `None` (REQ-614).
    ///
    /// Read from the session state the event stream folds into, so `/doctor`
    /// answers from what the client was actually told rather than by asking the
    /// daemon a second time — one fact, one source.
    #[must_use]
    pub fn pinned_cause(&self) -> Option<&str> {
        self.state.pinned.as_deref()
    }
}

/// The snapshot one `skills/list` reply becomes (REQ-585 ADR-2).
///
/// Pure, and split out of [`Connection::refresh_skills`] for exactly one
/// reason: the branch that matters is the one no daemon in a unit test can be
/// made to take on demand. A
/// [`METHOD_NOT_FOUND`](teton_protocol::jsonrpc::error_code::METHOD_NOT_FOUND)
/// reply is the **version handshake failing**, and it must read as *an empty
/// registry*, never as an error — that is what makes a new CLI against an old
/// daemon behave byte-for-byte as it does today.
///
/// Every other error reads empty too, and deliberately says nothing. A registry
/// the daemon could not produce is a session with no skills; a line about it at
/// every session start, and again at every `/cd`, would be chrome about a
/// capability the user may never have used — the same posture
/// [`Connection::answer_outstanding_model_proposal`] takes towards a daemon too
/// old to answer it.
#[must_use]
pub(crate) fn snapshot_from_skills_reply(
    reply: Result<methods::SkillsListResult, RpcError>,
) -> crate::slash::SkillSnapshot {
    match reply {
        Ok(result) => crate::slash::SkillSnapshot::from(result),
        Err(_) => crate::slash::SkillSnapshot::empty(),
    }
}

/// One message read off the socket.
enum Incoming {
    /// A response to one of our requests.
    Response(Response<Value>),
    /// A broadcast event (boxed to keep this enum small).
    Event(Box<teton_protocol::events::EventEnvelope>),
    /// The daemon evicted our subscription for lagging.
    Lagged(RpcError),
}

/// What woke the turn pump: something the daemon said, or the clock (REQ-621
/// ADR-621-1).
///
/// The pump's whole difficulty was that it had exactly one of these. A
/// `session/prompt` blocks on [`Connection::recv`] for the life of the turn, so
/// during a silent stretch — routing, a provider's queue, a running tool, a
/// model composing its next step — *nothing runs on the client at all*, and a
/// user watching an unmoving cursor cannot tell working from hung. A second
/// reason to wake is the whole mechanism: `Tick` is the client's own clock
/// arriving, which is the one fact about a turn the daemon cannot report.
enum Wake {
    /// The daemon said something.
    Message(Incoming),
    /// [`FRAME_INTERVAL`] elapsed with the daemon silent.
    Tick,
}

/// The pump's ownership of the one activity row (REQ-621 ADR-621-3).
///
/// **Local to a call, never on the state.** The row is drawn, repainted and
/// withdrawn by exactly one loop — the one that holds the clock — and every
/// other writer sees it already withdrawn. Held here rather than on
/// [`SessionState`] so that "is there a row on screen" cannot be read, or
/// answered, anywhere but the loop that put it there; [`Connection::call`]
/// owns the value and lends it to the pump, which is what makes BR-12's
/// close-out reachable from **every** exit path including the pump's own `?`
/// (ADR-621-4).
struct RowState {
    /// Whether this surface can carry a live row at all — the TTY gate,
    /// [`Surface::has_live_rows`], read **once** per call.
    ///
    /// Read once and carried rather than asked at each site, so there is one
    /// answer per call and one place a mutation of the gate shows up. Answered
    /// `false` the pump keeps the blocking receive it has always had and this
    /// row is inert, which is how BR-6's byte-identical piped output holds by
    /// construction rather than by a conditional at each draw.
    live: bool,
    /// Whether the **activity** row is on screen right now — the top row of
    /// the block, and **always exactly one row above the cursor** (REQ-622
    /// ADR-622-4).
    ///
    /// One, with a pending row and without, which is the geometry the cursor
    /// rule buys: the pending row is the row the cursor *sits on* rather than
    /// a row it has stepped past, so it adds no offset to the row above it.
    /// This was `2 or 1` until the verify pass, and arithmetic over two
    /// visibilities is exactly the kind of thing that goes wrong once —
    /// BUG-225 was a wrong offset and nothing else.
    ///
    /// The offset is still never written at a call site: [`Self::paint_rows`]
    /// and [`Self::withdraw_rows`] are the only two verbs that name one, and a
    /// third reader would be a third opinion about where the cursor is.
    activity_visible: bool,
    /// Whether the **pending** row — what the user is typing — is on screen.
    ///
    /// It is the bottom row of the block and **the row the cursor is on**: it
    /// is drawn with no trailing newline, so the terminal's caret rests at the
    /// end of the user's own text, which is where ADR-622-4 says it belongs and
    /// where anyone who has used a terminal expects it. Its three verbs are
    /// therefore [`Surface::draw_current_row`],
    /// [`Surface::repaint_current_row`] and
    /// [`Surface::withdraw_current_row`] — no offset in any of them, because
    /// there is no cursor motion to count.
    ///
    /// A row cannot be inserted above one already drawn ([`Surface::draw_row`]
    /// appends at the cursor), so a change in the activity row's *presence*
    /// while this row is up takes both rows down and redraws them in order.
    /// That reflow is in [`Self::paint_rows`] and is the reason the two
    /// visibilities are separate fields rather than a count.
    pending_visible: bool,
    /// Whether this call reads the keyboard, and what it holds in order to
    /// (REQ-622, verify).
    ///
    /// **A companion rather than four more fields here.** The block's own
    /// state is a geometry — which rows are on screen, at what width, at which
    /// animation frame — and owning the terminal's input is a different
    /// concern that merely has the same lifetime: it is taken at the top of a
    /// call and given back at its close-out, and the block reads exactly one
    /// bit of it ([`InputOwnership::owns_input`]) to decide whether the bottom
    /// row is its to draw. Keeping the guard, the two seams into the terminal
    /// and the verdict in one named value is what lets that bit be a question
    /// asked of something, rather than a flag the geometry happens to carry.
    ///
    /// Held on the row, and so owned by [`Connection::call`] and lent to the
    /// pump, for the guard's own reason (ADR-621-4, BR-7): the restore has to
    /// ride *every* way out of the call, including the ones nobody
    /// anticipated. See [`InputOwnership::guard`].
    input: InputOwnership,
    /// Which animation frame the row is showing. Advanced by a `Tick` only: a
    /// redraw after a durable line is not an animation step.
    tick: u64,
    /// The surface width the row is currently fitted to.
    ///
    /// **Re-read each time a row is drawn, and never on a repaint.** The two
    /// stale-width failures are not the same size. Between two frames of one
    /// row a resize costs a truncation a column early, and re-reading there
    /// would be a `TIOCGWINSZ` eight times a second for the whole of every
    /// turn. Between two *rows* — and every durable line the turn prints puts
    /// one row away and brings another back — a stale width fits the new row to
    /// a window that no longer exists, and a row wider than the terminal is
    /// hard-wrapped into a second row `withdraw_row_above(1)` cannot clear
    /// (BR-5). So the query is paid once per drawn row, which is where it buys
    /// something.
    width: usize,
    /// Where a drawn row's width comes from.
    ///
    /// [`crate::prompt::terminal_width`] in production. A field rather than a
    /// direct call so a test can resize a terminal it does not have: under
    /// `cargo test` `STDOUT_FILENO` is whichever terminal the developer happens
    /// to be sitting in, so a literal oracle over a real query would be
    /// asserting that terminal's truncation (the reason [`Self::at_width`]
    /// exists at all).
    measure_width: fn() -> usize,
    /// Whether a line the user has **submitted** is waiting to be read on
    /// stdin (BR-9).
    ///
    /// Consulted through a field for [`Self::measure_width`]'s reason and one
    /// sharper one: `cargo test` runs with stdin on `/dev/null`, where `poll`
    /// reports readable-at-EOF immediately, so the production answer inside a
    /// test build is a permanent "the user just pressed Enter" — every row in
    /// the suite abandoned before its second frame. [`Self::new`] therefore
    /// installs `|| false` under `cfg(test)`, and the one test that means
    /// "Enter was pressed" says so by setting this hook.
    line_waiting: fn() -> bool,
}

/// One call's hold on the terminal's input (REQ-622 ADR-622-1, BR-1, BR-13).
///
/// Extracted from [`RowState`] at REQ-622's verify pass, where the row had
/// grown two concerns: where its rows are on the screen, and whether this call
/// is the one reading the keyboard. They share a lifetime and nothing else —
/// the block is drawn on every live call, the input is taken only by a turn at
/// a terminal — and only one bit crosses between them
/// ([`Self::owns_input`], which is what gates the pending row).
struct InputOwnership {
    /// Whether this call reads the keyboard and owns the pending row.
    ///
    /// True exactly when [`Self::engage_input`] took the terminal out of
    /// canonical mode for this call. It is read, rather than
    /// [`crate::prompt::RawMode::is_engaged`], everywhere the pump decides
    /// whether to read bytes or draw a pending row: the process-wide answer is
    /// the right one for a *question's* reader (ADR-622-2, TASK-418) and the
    /// wrong one here, because it would still say yes on a nested call that
    /// never engaged anything and does not own the rows.
    ///
    /// False is REQ-621's world unchanged: the kernel assembles the line, the
    /// terminal echoes it, and [`RowState::abandon`] is the answer to a
    /// submitted line.
    engaged: bool,
    /// The guard that puts the terminal back, held for the pump's lifetime
    /// (ADR-622-1, BR-7).
    ///
    /// **Held here for `row`'s own reason** (ADR-621-4): [`Connection::call`]
    /// owns this value and lends it to the pump, so the restore rides every
    /// way out of the call — the `?` on the send, on each receive, on a result
    /// that fails to deserialize, on a dispatch that could not answer a
    /// permission, on a disconnected channel, and on a panic unwinding through
    /// the pump (AC-5). A guard bound in the pump instead would be dropped on
    /// all of those too, but a guard returned *by value* would not, and BR-7 is
    /// precisely a rule about the paths nobody anticipated.
    ///
    /// `None` on the canonical path, and `None` under a test hook — see
    /// [`InputHandover`], which is why the verdict is a separate field from the
    /// guard.
    guard: Option<RawMode>,
    /// How this call asks the terminal for its input.
    ///
    /// [`engage_raw_mode`] in production; a scripted verdict under test, for
    /// [`RowState::line_waiting`]'s reason and one sharper one. `cargo test`
    /// runs with `STDIN_FILENO` on whichever terminal launched it, so a real
    /// `tcsetattr` inside a unit test would take the *developer's* terminal
    /// out of canonical mode and — if the test then failed on an assertion,
    /// which is what a test is for — leave it there.
    engage: fn() -> InputHandover,
    /// Where the pump's keystrokes come from.
    ///
    /// [`crate::prompt::read_available`] in production — the **only** reader of
    /// stdin this module names, which is BR-2 as a source property and is swept
    /// for by `the_pump_reads_only_through_read_available`. A field for
    /// [`Self::engage`]'s reason: a test with no terminal has no way to make a
    /// keystroke arrive, and `/dev/null` under `cargo test` reports readable at
    /// EOF and then reads zero, so the production answer inside a test build is
    /// a permanent "nothing was typed".
    read_available: fn(&mut [u8]) -> io::Result<usize>,
}

/// How many bytes one look at the keyboard may take.
///
/// Several times a full pasted line, so the common case is one `read(2)`, and
/// small enough to sit on the pump's stack for the length of a tick. It is not
/// a limit on what a paste may contain: [`RowState::read_keys`] loops until the
/// descriptor has nothing left, so a larger block simply arrives in more than
/// one read (BR-6, AC-14).
const KEY_CHUNK: usize = 512;

/// BR-11's one notice: a real terminal refused to leave canonical mode.
///
/// Named rather than composed, and one sentence with the consequence in it — a
/// notice that said only "raw mode failed" would tell a user nothing they could
/// act on or recognise. Printed in verbose mode only, at the one site that
/// learns it (ADR-622-1): the turn runs either way, so this is a fact about the
/// terminal and not an error about the turn.
const RAW_REFUSED: &str = "input: this terminal refused to leave canonical mode; typing during \
                           the turn behaves as before, and a submitted line gives up the \
                           activity row";

impl InputOwnership {
    /// A call that has not taken the terminal, with its two seams installed.
    ///
    /// **Both arms of each `if` are compiled in every build**, which is not
    /// incidental: a `#[cfg(test)]` item cannot be named from an expression
    /// that also has to compile in release, and this runs inside the
    /// constructor [`Connection::call`] uses — so a test that drives the real
    /// `call` has no later moment at which to install a hook the way
    /// [`RowState::at_width`] does. The cost is two functions in the shipped
    /// binary that nothing there can reach; the alternative is BR-1's gate and
    /// BR-3's row having no test route that does not involve the developer's
    /// own terminal.
    fn new() -> Self {
        let engage: fn() -> InputHandover = if cfg!(test) {
            scripted_engage
        } else {
            engage_raw_mode
        };
        let read_available: fn(&mut [u8]) -> io::Result<usize> = if cfg!(test) {
            scripted_keys
        } else {
            crate::prompt::read_available
        };
        Self {
            engaged: false,
            guard: None,
            engage,
            read_available,
        }
    }

    /// Whether this call reads the keyboard and owns the pending row.
    ///
    /// A question asked of this value rather than a field the geometry carries
    /// — see [`Self::engaged`] for why the answer is this one and not
    /// [`crate::prompt::RawMode::is_engaged`].
    fn owns_input(&self) -> bool {
        self.engaged
    }

    /// Take the terminal's input for the length of this call, if this is a call
    /// that should have it (REQ-622 ADR-622-1, BR-1).
    ///
    /// Three conditions, and each of them is necessary. **`ends_turn`**,
    /// because raw mode belongs to a *turn*: the thirty non-turn RPCs this
    /// struct also serves finish in milliseconds, have no row and no pending
    /// line, and a `/cost` that flipped the terminal's mode would be a mode
    /// change with no reader behind it. It is asked of the **method** —
    /// `P::ENDS_TURN`, declared beside the wire name — and handed in rather
    /// than re-derived, for [`Connection::call`]'s reason: thirty call sites
    /// are thirty chances to answer it wrongly, so exactly one place answers
    /// it. **`at_a_live_surface`** is stdout's half of "at a terminal", read
    /// once from the surface's own gate and handed in by
    /// [`RowState::engage_input`]; **`typed_input`** is stdin's, threaded from
    /// the one edge that reads `IsTerminal`. A piped stdin with a terminal stdout
    /// therefore never reaches `tcsetattr` at all, which is AC-7 by
    /// construction rather than by a conditional at each draw.
    ///
    /// The three outcomes are not three errors. `NoTerminal` is the ordinary
    /// piped session and says nothing at all. `Failed` is a real terminal that
    /// refused: canonical mode stays, [`RowState::abandon`] stays armed, the
    /// turn runs, and one verbose notice names it (BR-11 — the opposite polarity
    /// from the key prompt's fail-closed refusal, because there is nothing here
    /// to hide). `Raw` hands over the guard whose `Drop` is the restore, kept
    /// on this struct so that **every** way out of the call puts the terminal
    /// back (BR-7, AC-5) — see [`Self::guard`].
    fn engage_input(&mut self, ctx: &mut UiContext, ends_turn: bool, at_a_live_surface: bool) {
        if !(ends_turn && at_a_live_surface && ctx.typed_input) {
            return;
        }
        let handover = (self.engage)();
        match handover.verdict {
            InputVerdict::Raw => {
                self.engaged = true;
                self.guard = handover.guard;
                ctx.state.input.set_owned(true);
                // BR-14's count is **seeded** here and not only updated on an
                // Enter, because the queue outlives a turn: it is drained one
                // line per pass of the entry loop (ADR-622-5), so a second line
                // typed during the last turn is still waiting while this one
                // runs — and `TurnActivity::begin` has just cleared the count
                // along with every other figure that must not be inherited. The
                // editor is asked, rather than a tally kept here, for
                // `set_queued`'s reason: one store of the lines, one count.
                ctx.state.activity.set_queued(ctx.state.input.queued_len());
            }
            // A session that was never at a terminal on stdin. Nothing was
            // changed, so there is nothing to say and nothing to undo.
            InputVerdict::NoTerminal => {}
            InputVerdict::Failed => {
                if ctx.state.verbose {
                    ctx.surface.line(LineKind::Info, RAW_REFUSED);
                }
            }
        }
    }

    /// Put the terminal back, at the close-out and before the call's last
    /// durable lines are written (BR-7).
    ///
    /// Dropping the guard **is** the restore — `RawMode`'s `Drop` writes the
    /// saved settings back with `TCSANOW` — so this exists for its *ordering*
    /// rather than for its effect: the value would be dropped a few lines
    /// later anyway when `row` goes out of scope. Two reasons to be explicit.
    /// The rows are withdrawn first, so the terminal is still the terminal the
    /// pump has been painting while it takes its rows back; and the turn's
    /// closing lines — the hand-off, the cost line, a refusal — are written in
    /// canonical mode, which is the mode the entry prompt that follows them
    /// reads in.
    ///
    /// `engaged` goes with it: the input is no longer this call's, and a
    /// paint after this point must not draw a pending row the pump can no
    /// longer keep up to date.
    fn release(&mut self) {
        self.engaged = false;
        self.guard = None;
    }

    /// Read whatever the user has typed since the last look and fold it into
    /// the editor (BR-2, BR-3).
    ///
    /// **The pump is the reader.** This is the whole of REQ-622's first
    /// direction: the tick that already asked whether stdin had bytes now takes
    /// them, through the one seam this module names
    /// ([`crate::prompt::read_available`]), on the thread that has always been
    /// the single reader of stdin (ADR-556-1). The kernel's line discipline is
    /// no longer in front of it, so there is still exactly one reader.
    ///
    /// Loops until the descriptor has nothing left, because one `read(2)`
    /// returns one buffer's worth: a pasted block larger than [`KEY_CHUNK`]
    /// arrives in several reads and must be one paste, not the first 512 bytes
    /// of one (AC-14). The loop is bounded by what the kernel is holding — a
    /// look that finds nothing reports zero, which is how it ends.
    ///
    /// A read error costs this tick's keystrokes and nothing else. `EINTR` is
    /// already folded into "nothing waiting" by `read_available`, so what is
    /// left is a descriptor that has genuinely gone wrong; the next tick asks
    /// again, and failing a turn over it would be the tradeoff BR-11 rejects at
    /// the other end of the same seam.
    ///
    /// **Only `Queued` is acted on here.** `Pending` and `Nothing` need no
    /// bookkeeping because [`RowState::paint_rows`] is a *projection* of the
    /// editor and the activity, and it runs immediately after every read — a "the row
    /// is stale" flag beside it would be a second opinion about what is on
    /// screen. A queued line is different in kind: it changes a count on the
    /// **activity** row (BR-14), and the editor is the only thing that knows
    /// what that count now is.
    fn read_keys(&mut self, ctx: &mut UiContext) {
        if !self.engaged {
            return;
        }
        let mut buf = [0u8; KEY_CHUNK];
        loop {
            let read = match (self.read_available)(&mut buf) {
                Ok(0) | Err(_) => return,
                // Clamped rather than trusted: production cannot report more
                // than the buffer it was handed, and a test hook that did would
                // panic the slice rather than fail an assertion.
                Ok(read) => read.min(buf.len()),
            };
            for edit in ctx.state.input.push(&buf[..read]) {
                match edit {
                    Edit::Queued(waiting) => ctx.state.activity.set_queued(waiting),
                    Edit::Pending | Edit::Nothing => {}
                }
            }
        }
    }
}

impl RowState {
    /// The fields neither constructor has an opinion about.
    ///
    /// The five constant fields, in **one** place: `new` and
    /// [`Self::at_width`] each differ from this in one or three fields and used
    /// to restate all eight, which is eight chances for the fixture and the
    /// real constructor to drift — a `pending_visible: true` in one of them
    /// would be a test fixture that believes a row is on screen before anything
    /// has been drawn ([[LESSON-547]]: one source of truth for a shape two
    /// callers share).
    fn base() -> Self {
        // A fn pointer either way, and the annotation is what lets the two
        // arms coerce to one. `|| false` under test is not a convenience: see
        // the field.
        let line_waiting: fn() -> bool = if cfg!(test) { || false } else { line_submitted };
        Self {
            live: false,
            activity_visible: false,
            pending_visible: false,
            input: InputOwnership::new(),
            tick: 0,
            // Not asked here. The query is an `ioctl` on `STDOUT_FILENO`, every
            // non-turn RPC comes through this constructor too — some thirty of
            // them — with nothing to measure, and a width read now would be a
            // width from before the first frame anyway. `paint_rows` asks when
            // it has a row to fit.
            width: 0,
            measure_width: crate::prompt::terminal_width,
            line_waiting,
        }
    }

    /// The row's state at the start of a call: nothing drawn, gate and width
    /// taken from the surface.
    fn new(surface: &dyn Surface) -> Self {
        Self {
            live: surface.has_live_rows(),
            ..Self::base()
        }
    }

    /// Whether this call reads the keyboard and owns the pending row — the one
    /// bit of [`InputOwnership`] the block's geometry reads (BR-1, BR-13).
    fn owns_input(&self) -> bool {
        self.input.owns_input()
    }

    /// Give up the rows for the rest of this call, leaving the screen exactly
    /// as it is (REQ-621 BR-9, added at verify).
    ///
    /// **REQ-622 amendment: reachable only where this call did *not* take the
    /// input.** With [`Self::owns_input`] true there is no canonical mode and
    /// no kernel echo, so a submitted line moves nothing the pump cannot see —
    /// Enter is a byte this loop read, the line goes to the editor's queue, and
    /// the pending row comes down under [`Self::paint_rows`]' own bookkeeping.
    /// The whole geometry problem below is a property of the mode, so the
    /// fallback survives for exactly the two paths that still have that mode:
    /// a terminal that refused it (BR-11) and a session that never asked
    /// (piped stdin, a non-turn RPC). The other caller is BR-13's failed write,
    /// which is about a terminal that stopped taking bytes and is unrelated to
    /// either mode.
    ///
    /// **Why a row is ever abandoned rather than withdrawn.** The row's whole
    /// geometry is the sentence "it is one row above the cursor", and that
    /// sentence is true only while nothing but this loop moves the cursor. A
    /// submitted line moves it: stdin is in canonical mode for the whole of a
    /// turn with `ECHO` on, so the terminal — not this process — echoes the
    /// user's characters into the row below the row, and echoes a newline when
    /// they press Enter. The cursor drops a line under bookkeeping a
    /// canonical-mode client cannot see (there is no read, no `\n` of ours, and
    /// nothing on the wire), and from that moment the row is *two* above the
    /// cursor.
    ///
    /// So both verbs are now wrong, and wrong in the way that matters:
    /// `repaint_row_above(1)` would rewrite the line holding the characters the
    /// user just typed — the blanking BR-9 forbids in as many words — and
    /// `withdraw_row_above(1)` would erase that line outright, which is worse
    /// than a stale row by any measure. Writing nothing leaves the last frame
    /// in scrollback: a bounded, recorded exception to BR-5 (requirement BR-5,
    /// amended 2026-09-10), one row per turn, and the only option here that
    /// does not damage something the user typed.
    ///
    /// Dropping `live` as well as `visible` is what makes it "for the rest of
    /// the turn": the pump reads `live` to decide whether to wait with a
    /// timeout at all, so an abandoned row also stops the ticking it can no
    /// longer paint with.
    fn abandon(&mut self) {
        self.activity_visible = false;
        self.pending_visible = false;
        self.live = false;
    }

    /// Take the terminal's input for the length of this call (REQ-622
    /// ADR-622-1, BR-1), and hold the guard that gives it back.
    ///
    /// The decision is [`InputOwnership::engage_input`]'s; what the row
    /// contributes is stdout's half of "at a terminal", which is the gate it
    /// read once from the surface. Kept as a method on the row so the one
    /// caller — [`Connection::call`], which alone knows whether the method is
    /// a turn — has one value to talk to.
    fn engage_input(&mut self, ctx: &mut UiContext, ends_turn: bool) {
        self.input.engage_input(ctx, ends_turn, self.live);
    }

    /// Put the terminal back, at the close-out and before the call's last
    /// durable lines are written (BR-7). [`InputOwnership::release`]'s, and
    /// delegated for [`Self::engage_input`]'s reason.
    fn release_raw(&mut self) {
        self.input.release();
    }

    /// Read whatever the user has typed since the last look and fold it into
    /// the editor (BR-2, BR-3). [`InputOwnership::read_keys`]'s.
    fn read_keys(&mut self, ctx: &mut UiContext) {
        self.input.read_keys(ctx);
    }

    /// Make what is on screen match the block the two projections would draw
    /// now (REQ-621 ADR-621-3, REQ-622 ADR-622-4).
    ///
    /// The block is two rows in a fixed order — the activity row, and beneath
    /// it the line the user is typing — and either may be absent. Its whole
    /// discipline is here, in one place, so the tick arm and the
    /// after-a-message redraw cannot come to disagree, and so **no call site
    /// ever writes a row offset**: the pending row is the row the cursor is on
    /// and the activity row is the one above it, with a pending row and
    /// without, so the only offset in the block is `1` (BR-3, BR-13).
    ///
    /// It runs in two passes for a reason that is a property of the terminal
    /// rather than a choice: every draw verb appends at the cursor, so a row
    /// cannot be *inserted* above a row already on screen. So the teardown pass
    /// goes bottom-up and the draw pass top-down, and a change in the activity
    /// row's presence while a pending row is up takes the pending row down with
    /// it and redraws both. (`withdraw_row_above` leaves the cursor on the row
    /// it cleared, so the redraw lands exactly where the block was — the rows
    /// move up by however many left, and nothing above the block is touched.)
    ///
    /// | activity | pending | done |
    /// |---|---|---|
    /// | `Some`, on screen | unchanged | `repaint_row_above(1, ..)` — in place |
    /// | `Some`, not on screen | — | `draw_row` — the row scrolls in beneath whatever was last written |
    /// | `None`, on screen | taken down first | `withdraw_row_above(1)` — gone without residue |
    /// | `Some` ⇄ `None` | on screen | pending withdrawn, then both drawn in order |
    ///
    /// and the pending row, one line down and one verb over in each case:
    /// `repaint_current_row` in place, `draw_current_row` at the cursor,
    /// `withdraw_current_row` to take it back. Every one of them acts on the
    /// row the cursor is already on, which is what it means for that row to be
    /// the current one.
    ///
    /// Gated on [`Self::live`], so a piped surface reaches no verb here at all
    /// (REQ-621 BR-6); the pending row is gated on [`Self::owns_input`] as
    /// well, because a client that is not the one echoing has no business
    /// drawing what the kernel is already painting. Whether there is an
    /// activity row is then the **projection's** decision and never the
    /// method's: a `/cost` pumping through this loop finds the activity idle
    /// and paints nothing (ADR-621-1).
    ///
    /// **Both rows are drawn with [`Surface::draw_row`], never `line`** (REQ-622
    /// BR-4). A durable line flushes whatever the renderer is holding ahead of
    /// itself, which is right for a durable line and wrong for a row this loop
    /// is about to take back: the block is redrawn after every streamed token,
    /// so a draw that flushed ended the streamed line at every token — a reply
    /// typed past came out one word per row where the same reply with nothing
    /// typed was one. The held line goes out where the block was, by the
    /// durable write or the turn's end that follows the next withdraw.
    fn paint_rows(&mut self, ctx: &mut UiContext, now: Instant) {
        if !self.live {
            return;
        }
        // A row about to be **drawn** is fitted to the terminal as it is now,
        // and a repaint keeps the width its row was drawn at — see
        // [`Self::width`] for why those are different decisions. The two
        // existence questions are asked before the width, and asked of the
        // *content*: `has_row` is the predicate `frame` answers with
        // (ADR-621-1), and the pending row's existence is a question about the
        // editor rather than about the terminal, so it is asked at a width no
        // terminal can be narrower than. That ordering is what keeps the
        // `ioctl` on the path where a row is genuinely due, rather than eight
        // times a second through a whole streamed reply.
        let activity_due = ctx.state.activity.has_row(now);
        // BR-14's count, on the row that is on screen when the activity row is
        // not (REQ-622, verify). `frame` answers `None` for the whole of
        // `Streaming` — a reply arriving is its own feedback, ADR-621-1 — so an
        // Enter pressed while the model was writing withdrew the pending row
        // and said nothing at all, which is what a swallowed keystroke looks
        // like. `None` here means "the activity row is carrying the count", so
        // the two can never both say it.
        let queued_hint = (!activity_due).then(|| ctx.state.input.queued_len());
        let pending_due =
            self.owns_input() && ctx.state.input.row(usize::MAX, queued_hint).is_some();
        // A row about to be drawn is fitted to the terminal as it is now, and
        // the pending row is drawn again whenever the activity row **leaves**
        // as well as when it appears: a row cannot be inserted above one
        // already on screen, so either change takes the pending row down and
        // puts it back (the reflow below). The appearing half is the first
        // clause; the leaving half had no clause until the verify pass, so a
        // terminal resized during a turn redrew the pending row at the width it
        // had before — a row wider than the window, hard-wrapped into a second
        // row the withdraw cannot clear, which is the failure [`Self::width`]
        // is written against arriving by the one door nobody watched.
        let activity_leaves = self.activity_visible && !activity_due;
        // ...and once a second while any row is up (verify, Step D): a resize
        // during a tick-only stretch — the model thinking, nothing arriving —
        // would otherwise repaint at the old width until the next message.
        let periodic =
            self.tick.is_multiple_of(8) && (self.activity_visible || self.pending_visible);
        if (!self.activity_visible && activity_due)
            || (!self.pending_visible && pending_due)
            || (self.pending_visible && activity_leaves)
            || periodic
        {
            self.width = (self.measure_width)();
        }
        let activity = ctx.state.activity.frame(now, self.tick, self.width);
        let pending = if self.owns_input() {
            ctx.state.input.row(self.width, queued_hint)
        } else {
            None
        };

        // ---- Teardown, bottom row first.
        //
        // The pending row goes if it should not be there — and also if the
        // activity row is about to appear or disappear, since the row above it
        // cannot change while it is on screen.
        let activity_appears_or_leaves = activity.is_some() != self.activity_visible;
        if self.pending_visible && (pending.is_none() || activity_appears_or_leaves) {
            // No offset: the cursor is on this row. What the clear leaves is
            // the cursor at column 0 of an empty row directly under the
            // activity row — exactly the state the block was in before the
            // pending row was ever drawn, which is why the offset above it is
            // `1` either way.
            let cleared = ctx.surface.withdraw_current_row();
            self.pending_visible = false;
            if !cleared {
                self.hide_after_a_failed_write(ctx);
                return;
            }
        }
        if self.activity_visible && activity.is_none() {
            let cleared = ctx.surface.withdraw_row_above(1);
            self.activity_visible = false;
            if !cleared {
                self.hide_after_a_failed_write(ctx);
                return;
            }
        }

        // ---- Draw, top row first.
        if let Some(text) = activity {
            if self.activity_visible {
                // One, with a pending row beneath and without: see
                // [`Self::activity_visible`]. The repaint's own save/restore
                // pair is what puts the cursor back on the end of the pending
                // row afterwards.
                if !ctx.surface.repaint_row_above(1, LineKind::Activity, &text) {
                    self.hide_after_a_failed_write(ctx);
                    return;
                }
            } else {
                // The one write here that reports nothing: `draw_row` has
                // `line`'s signature — the seam every durable write shares,
                // infallible — so a draw whose bytes did not land still marks
                // the row visible. That is the asymmetry BR-13 leaves — the
                // fallible verbs are the two that move the cursor — and it
                // self-corrects on the next tick: a stdout that dropped the
                // draw drops the repaint too, and *that* verb reports it.
                ctx.surface.draw_row(LineKind::Activity, &text);
                self.activity_visible = true;
            }
        }
        if let Some(text) = pending {
            if self.pending_visible {
                // The block's last verb, so a refusal is reported and there is
                // nothing after it to guard against. `\r`, erase, write: the
                // cursor is on this row and is meant to end up back at the end
                // of it, so there is nothing to save and nothing to restore.
                if !ctx.surface.repaint_current_row(LineKind::Pending, &text) {
                    self.hide_after_a_failed_write(ctx);
                }
            } else {
                // Drawn **without** a trailing newline, which is the whole of
                // ADR-622-4's cursor rule: the caret stays at the end of what
                // the user is typing instead of parking on the blank row below
                // the block.
                ctx.surface.draw_current_row(LineKind::Pending, &text);
                self.pending_visible = true;
            }
        }
    }

    /// Take the whole block off the screen, bottom row first, and report
    /// whether every byte landed (REQ-622 ADR-622-4).
    ///
    /// The withdraw-before-anything-else rule, extended from one row to the
    /// block: a durable line — a tool's `[running]`, a notice, a permission
    /// question — always prints where the block was, and the block comes back
    /// beneath it. Bottom first because both clearing verbs leave the cursor at
    /// the start of the row they cleared: the pending row is cleared where the
    /// cursor already is, which puts the cursor on the row directly under the
    /// activity row, and the activity row is then one up from there.
    ///
    /// Stops at the first refused write and reports it. A withdraw whose bytes
    /// did not land did not move the cursor either, so the offset the second
    /// one would use is measured from a row it is no longer sitting under —
    /// there is nothing useful left to try. Both rows are marked gone whatever
    /// the verb reported, for [`Self::paint_rows`]' reason: going on believing
    /// a row is ours would put the next repaint over whatever is there now.
    ///
    /// The report is the caller's to act on, because the two callers owe
    /// different things. The pump has a whole turn left and gives up the block
    /// with a notice ([`Self::hide_after_a_failed_write`]); `call`'s close-out
    /// is returning, and a verbose line there would be news about a row that is
    /// already out of scope.
    fn withdraw_rows(&mut self, ctx: &mut UiContext) -> bool {
        if self.pending_visible {
            // The cursor is on this row, so the clear takes no offset; what it
            // leaves is the cursor at the start of the row, which is where the
            // activity row's `withdraw_row_above(1)` measures from.
            let cleared = ctx.surface.withdraw_current_row();
            self.pending_visible = false;
            if !cleared {
                self.activity_visible = false;
                return false;
            }
        }
        if self.activity_visible {
            let cleared = ctx.surface.withdraw_row_above(1);
            self.activity_visible = false;
            return cleared;
        }
        true
    }

    /// BR-13: a terminal that would not take the block's bytes costs the turn
    /// its rows, and nothing else.
    ///
    /// **Never fatal, never silent** — the rule has two halves and the second
    /// is the one that is easy to leave out. The rows are given up for the rest
    /// of the call (the state [`Self::abandon`] leaves, reached for a different
    /// reason), and in verbose mode one durable line says so. Silence would
    /// make a write failure indistinguishable from a daemon that went quiet,
    /// which is exactly the ambiguity BR-11's stall annotation exists to remove
    /// — a user watching a stopped row would read "the daemon is wedged" from a
    /// fact about their terminal.
    ///
    /// One line, not one per frame: `abandon` is what makes that structural
    /// rather than a counter, since the next paint returns before it reaches a
    /// verb.
    fn hide_after_a_failed_write(&mut self, ctx: &mut UiContext) {
        // The pending row is the cursor's own row, so a notice written now
        // would land on the user's text and drag the held paragraph with it
        // (`line` emits what is held). Clear the row first; a clear that also
        // fails leaves the cursor where it is, which is no worse.
        if self.pending_visible {
            let _ = ctx.surface.withdraw_current_row();
        }
        self.abandon();
        if ctx.state.verbose {
            ctx.surface.line(
                LineKind::Info,
                "activity row: terminal write failed; hidden for the rest of the turn",
            );
        }
    }

    /// A row over a live surface at a **fixed** width, for a test whose oracle
    /// is the row's own text (REQ-621 TASK-411).
    ///
    /// [`Self::new`] queries `STDOUT_FILENO`, which under `cargo test` is
    /// whichever terminal the developer happens to be sitting in — a literal
    /// oracle built on it would be asserting that terminal's truncation, and
    /// would go red on a narrow one for a reason having nothing to do with the
    /// pump. The fit itself is `activity.rs`'s to test, and it is tested there.
    #[cfg(test)]
    fn at_width(width: usize) -> Self {
        set_test_width(width);
        Self {
            live: true,
            width,
            // The same answer a redraw will get, so a fixture's rows are all
            // fitted to one width unless the test resizes on purpose
            // (`set_test_width`).
            measure_width: test_width,
            // The two seams are the scripted ones already: `InputOwnership::new`
            // chooses on `cfg!(test)`, so a fixture and the real `call` reach
            // the same stand-ins by the same route rather than by two lists
            // that could disagree.
            ..Self::base()
        }
    }

    /// The same row, on a call that took the terminal's input (REQ-622).
    ///
    /// A builder rather than a second constructor, and it deliberately does
    /// **not** call [`Self::engage_input`]: this is the state *after* a
    /// successful engage, which is what a test about the two rows wants, while
    /// a test about the engage itself drives `engage_input` with a scripted
    /// verdict (`a_refused_raw_mode_falls_back_to_abandon_and_says_so`) or
    /// drives the real [`Connection::call`]
    /// (`raw_mode_is_engaged_only_for_a_turn_at_a_terminal`).
    /// [`InputOwnership::guard`]
    /// stays `None` — there is no terminal in the room to restore, and only
    /// `prompt.rs` can build the guard.
    #[cfg(test)]
    fn owning_input(mut self) -> Self {
        self.input.engaged = true;
        self
    }
}

/// Whether a line the user has **submitted** is waiting to be read (BR-9).
///
/// `poll` with a zero timeout — the same non-blocking question the entry loop
/// asks once a frame — so consulting it costs the pump no latency, which is
/// BR-9's first clause and not something to spend on its second.
///
/// **At a terminal, "stdin has something" is "a line was submitted".** The
/// session leaves stdin in canonical mode, so the kernel makes no byte readable
/// until Enter; the readable descriptor *is* the event that moved the cursor
/// out from under the row (see [`RowState::abandon`]). Asked only where
/// `UiContext::typed_input` says this process's stdin is a terminal: a piped or
/// closed stdin reports readable at EOF, and reading that as a keystroke would
/// abandon a row nobody typed over.
///
/// EOF, an `EINTR`, or a `poll` error are all folded into the same answer the
/// entry loop takes them as, because the consequence here is the same either
/// way: at worst one row is given up a little early.
fn line_submitted() -> bool {
    crate::prompt::stdin_ready(Duration::ZERO)
}

/// [`crate::prompt::RawOutcome`] with the guard carried beside the verdict.
///
/// The one departure from ADR-622-1's shape, and it is a testing constraint
/// rather than a design choice: only `prompt.rs` can build a [`RawMode`] — its
/// saved `termios` is private, and rightly so — so a hook standing in for
/// `engage` could not return `RawOutcome::Raw(..)` at all. Splitting the
/// verdict from the guard lets a test say "the terminal went raw" with no
/// terminal in the room, while production carries the real guard in the same
/// value and hands it straight to [`RowState::raw`].
struct InputHandover {
    /// Which of the three states [`RowState::engage_input`] landed in.
    verdict: InputVerdict,
    /// The guard whose `Drop` restores the terminal. `Some` only on
    /// production's `Raw` path.
    guard: Option<RawMode>,
}

/// [`crate::prompt::RawOutcome`]'s three states, guard-free.
///
/// Mirrored here rather than reused because the enum that carries the guard
/// cannot be constructed outside `prompt.rs`; `engage_raw_mode` is the one
/// place the two spellings meet, which is what keeps this from being a second
/// opinion about what the terminal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputVerdict {
    Raw,
    NoTerminal,
    Failed,
}

/// Ask the terminal to leave canonical mode — the production hook (ADR-622-1).
fn engage_raw_mode() -> InputHandover {
    match RawMode::engage() {
        RawOutcome::Raw(guard) => InputHandover {
            verdict: InputVerdict::Raw,
            guard: Some(guard),
        },
        RawOutcome::NoTerminal => InputHandover {
            verdict: InputVerdict::NoTerminal,
            guard: None,
        },
        RawOutcome::Failed => InputHandover {
            verdict: InputVerdict::Failed,
            guard: None,
        },
    }
}

// The scripted terminal a test hands the pump: one answer for `engage`, a tally
// of the times it was asked, and a queue of keystroke bursts for
// `read_available`.
//
// **Compiled in every build and reachable only under `cfg!(test)`** —
// `RowState::new` says why, and it is `line_waiting`'s `|| false` one size up.
// The tally is the honest way to assert AC-7's "no termios call is made": the
// absence of a row would also pass against a pump that asked for the terminal
// and got nothing.
thread_local! {
    static SCRIPTED_VERDICT: std::cell::Cell<InputVerdict> =
        const { std::cell::Cell::new(InputVerdict::NoTerminal) };
    static SCRIPTED_ENGAGE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SCRIPTED_KEYS: std::cell::RefCell<std::collections::VecDeque<Vec<u8>>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

/// [`RowState::engage`] under test: the scripted verdict, and never a guard.
///
/// The default is [`InputVerdict::NoTerminal`], which is what every test that
/// says nothing about the keyboard means — the piped session REQ-621's fixtures
/// were all written against.
fn scripted_engage() -> InputHandover {
    SCRIPTED_ENGAGE_CALLS.with(|calls| calls.set(calls.get() + 1));
    InputHandover {
        verdict: SCRIPTED_VERDICT.with(std::cell::Cell::get),
        guard: None,
    }
}

/// [`RowState::read_available`] under test: one scripted burst per look.
///
/// One burst and then nothing, which is the shape of the real thing: a `poll`
/// that says yes, one `read(2)`, and a second look that finds the descriptor
/// empty. `script_keystrokes` is what puts the "and then nothing" in the queue.
fn scripted_keys(buf: &mut [u8]) -> io::Result<usize> {
    SCRIPTED_KEYS.with(|keys| {
        let Some(burst) = keys.borrow_mut().pop_front() else {
            return Ok(0);
        };
        let take = burst.len().min(buf.len());
        buf[..take].copy_from_slice(&burst[..take]);
        Ok(take)
    })
}

/// Hand the terminal to a question and take it back (REQ-622 BR-5).
///
/// **The type-ahead seam, and there is exactly one of them.** A question opened
/// mid-turn must read only keystrokes typed after it was drawn, and the line
/// the user was part-way through must come back verbatim afterwards. Both
/// halves are the editor's `shelve`/`unshelve` pair; what this function adds is
/// that they wrap **every** call that reaches a [`crate::prompt::Prompter`],
/// rather than being written out three times beside three questions where one
/// of them would eventually be added without them (LESSON-502).
///
/// The shelve is before the prompter is *called*, not before the question is
/// composed, because the callee renders the question and then reads: the fresh
/// buffer has to be in place across both, or the first keystroke of the answer
/// would land in the user's own line.
///
/// Nothing is taken out of the fresh buffer here. The answer belongs to the
/// prompter, which reads it through its own editor (ADR-622-2, TASK-418), and
/// `unshelve` discards whatever is left rather than merging it — a stray byte
/// typed at a question is not part of the sentence it interrupted.
///
/// Ungated, and deliberately so. With no raw mode engaged the editor's pending
/// line is empty, so this is a shelve of nothing and a restore of nothing; the
/// alternative is a reachability argument ("a question can only open inside a
/// turn, and a turn at a terminal owns the input") standing between the rule
/// and the code, which is what BR-5 exists to not depend on.
fn around_a_question<'a, T>(
    ctx: &mut UiContext<'a>,
    ask: impl FnOnce(&mut UiContext<'a>) -> T,
) -> T {
    ctx.state.input.shelve();
    // And the same rule one layer down, where the editor cannot reach (REQ-622,
    // verify). The shelve accounts for every byte the *pump* has read — and the
    // pump has just read everything the descriptor was holding, on the tick
    // immediately before this dispatch, because that is what `read_keys` does:
    // it loops until the descriptor reports nothing left. So anything still in
    // the kernel's input queue at this instant arrived in the window between
    // that read and this question's first row, which is the one span of time in
    // which a keystroke can be neither in the editor nor aimed at a question the
    // user can see. BR-5 says a question reads only what was typed after it was
    // drawn; the shelve says that of the editor, and this says it of the kernel.
    //
    // Only while the pump owns the input (verify, Step D): `dispatch_event` is
    // shared with the idle drain, where the terminal is canonical and the
    // kernel's queue *is* the line the user is typing at the entry prompt —
    // echoed, unsubmitted, and theirs. Flushing there would eat it.
    if ctx.state.input.is_owned() {
        crate::prompt::discard_type_ahead();
    }
    let answered = ask(ctx);
    ctx.state.input.unshelve();
    answered
}

/// Whether the debug-only mid-turn panic seam is armed (REQ-622 AC-5).
///
/// **Read once**, because it is read on the pump's tick and the answer cannot
/// change within a process; and gated by [`crate::slash::test_seams_allowed`],
/// so it is a **debug build with `TETON_TEST_SEAMS=1`** and nothing else.
///
/// That function's invariant asks each consumer to state its polarity, and this
/// one is the safe direction: the switch can only *add* a panic, so a release
/// build that ignores it declines to panic and keeps the stricter behaviour.
/// A shipped binary cannot be made to abort a turn by an environment variable.
///
/// The seam exists because AC-5 has a leg no other path can reach: a client
/// panicking mid-turn must still leave the terminal as it found it, and the
/// only honest way to test that at a real pty is to panic a real client at a
/// real pty. It is deliberately in the tick arm rather than at the top of the
/// turn — the terminal is raw and a row is on screen by then, which is the
/// state the restore has to survive.
fn panic_mid_turn_armed() -> bool {
    static ARMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ARMED.get_or_init(|| {
        crate::slash::test_seams_allowed()
            && std::env::var("TETON_TEST_PANIC_MID_TURN").ok().as_deref() == Some("1")
    })
}

/// What one dispatched event did, beyond arriving.
///
/// `rendered` was the whole of this and was a bare `bool`; the second field is
/// what the activity row needs (REQ-621 ADR-621-3). Reported rather than
/// re-derived: "the phase is `awaiting_permission`" is true of every event that
/// arrives *while* a question is on screen — a cost row, a lifecycle stage —
/// so a pump that read the phase instead would restore a question nobody had
/// answered. Only the arm that actually sent a reply knows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Dispatched {
    /// Whether the envelope rendered. An envelope the own-session filter drops
    /// renders nothing, which is what a caller counting paints is counting.
    rendered: bool,
    /// Whether **this** client answered a permission question for it. False on
    /// the arm that only reports another session's question, which is not ours
    /// to answer and whose phase we never entered.
    answered_permission: bool,
}

/// What one non-blocking drain of the event channel produced (REQ-556 BR-1).
///
/// Deliberately just a count. Lifecycle stages are folded into
/// [`SessionState::loading`] by `render_event`, which every event passes
/// through — returning them here as well would be a second path to the same
/// fact, and the two would drift the first time one of them was updated.
#[derive(Debug, Default)]
pub struct Drained {
    /// How many things actually rendered. Zero means the caller's screen is
    /// untouched, so an open entry frame is still intact.
    pub rendered: usize,
}

/// A live connection to the daemon.
pub struct Connection {
    writer: UnixStream,
    incoming: Receiver<Incoming>,
    next_id: i64,
    /// The daemon build version this connection handshook with (REQ-565 BR-6).
    ///
    /// Kept on the connection rather than returned to each caller because the
    /// skew warning belongs to *attaching*, and every command attaches through
    /// `ensure_connected`. `None` until the handshake completes.
    daemon_version: Option<String>,
    /// The daemon *name* the same handshake reported (REQ-582 BR-7).
    ///
    /// Beside the version because `/doctor` renders the daemon line from this
    /// connection rather than from a second handshake, and the shell twin's line
    /// names both. Hardcoding `teton-code` in the session's copy of that line
    /// would be a second source of truth for something the daemon states about
    /// itself. `None` until the handshake completes.
    daemon_name: Option<String>,
    /// The scripted double's hold on the timed receive, and its tally
    /// (REQ-621 TASK-411). Test-only.
    #[cfg(test)]
    timed: TimedReceive,
}

impl Connection {
    /// Open a connection to the daemon socket and start the reader thread.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the socket cannot be reached (no daemon).
    pub fn connect(socket: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let reader_stream = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("teton-reader".to_owned())
            .spawn(move || reader_loop(reader_stream, &tx))?;
        Ok(Self {
            daemon_version: None,
            daemon_name: None,
            writer: stream,
            incoming: rx,
            next_id: 1,
            #[cfg(test)]
            timed: TimedReceive::default(),
        })
    }

    /// The daemon build version this connection handshook with, once it has
    /// (REQ-565 BR-6).
    #[must_use]
    pub fn daemon_version(&self) -> Option<&str> {
        self.daemon_version.as_deref()
    }

    /// The daemon name this connection handshook with, once it has (REQ-582
    /// BR-7 — the in-session `/doctor` line).
    #[must_use]
    pub fn daemon_name(&self) -> Option<&str> {
        self.daemon_name.as_deref()
    }

    /// Perform the protocol-version handshake. No events precede it (the daemon
    /// subscribes a client only after a successful handshake), so this simply
    /// waits for the matching response.
    ///
    /// # Errors
    ///
    /// Returns an error if the handshake is rejected or the connection drops.
    pub fn handshake(&mut self) -> anyhow::Result<HandshakeResult> {
        let params = HandshakeParams {
            client_kind: ClientKind::Cli,
            client_name: CLIENT_NAME.to_owned(),
            client_version: CLIENT_VERSION.to_owned(),
            protocol_min: PROTOCOL_VERSION_MIN,
            protocol_max: PROTOCOL_VERSION_MAX,
            // The CLI drives one session and renders that session's stream;
            // monitoring is for tools that watch every session (REQ-568 ADR-C).
            monitor: false,
        };
        let id = self.send(params)?;
        loop {
            // No events precede the handshake, so anything but the matching
            // response is ignored.
            if let Incoming::Response(resp) = self.recv()? {
                if resp.id == id {
                    return match resp.error {
                        Some(err) => Err(explain_handshake_failure(err)),
                        None => {
                            let result: HandshakeResult =
                                serde_json::from_value(resp.result.unwrap_or(Value::Null))
                                    .map_err(|e| stale_daemon_hint(HandshakeParams::METHOD, &e))?;
                            // REQ-565 BR-6: remembered here so the build-skew
                            // notice is derived from the handshake that actually
                            // happened, rather than from a second query that
                            // could reach a different daemon.
                            self.daemon_version = Some(result.daemon_version.clone());
                            self.daemon_name = Some(result.daemon_name.clone());
                            Ok(result)
                        }
                    };
                }
            }
        }
    }

    /// Send a request, pump events until its response arrives, and return either
    /// the typed result or the daemon's [`RpcError`]. Transport/parse failures
    /// surface as the outer `anyhow::Error`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection drops or a payload fails to (de)serialize.
    pub fn call<P: RpcMethod>(
        &mut self,
        params: P,
        ctx: &mut UiContext,
    ) -> anyhow::Result<Result<P::Result, RpcError>> {
        // REQ-592 BR-8 / ADR-3. **The only `end_block()` in the client**, on the
        // one branch where the RPC that just finished was a *turn*.
        //
        // This comment used to say that `call` returning *is* the turn boundary.
        // It is not, and the gap was found at review: `call` is the **RPC**
        // boundary, and roughly thirty of its callers are not turns — every
        // slash handler in `main.rs` (`/cost`, `/model`, `/config`), the
        // provider setup and connection-test flows, `answer_outstanding_model_
        // proposal`'s own `model/status` probe. A fence cleared by any of those
        // re-flows the rest of a second client's streaming ` ```rust ` block as
        // prose the moment this user types a command: BR-6's failure arriving at
        // user-command frequency instead of at the poll rate ADR-4's third call
        // site was dropped for.
        //
        // So the fence is dropped on `P::ENDS_TURN` — a property of the
        // **method**, declared beside its wire name, true for `session/prompt`
        // and nothing else. Making it a property of the call would have left
        // thirty sites each able to answer it wrongly, which is the shape of
        // rule this REQ has already had to unpick twice. Every other method
        // flushes and leaves block state alone, for `emit_held`'s reason below.
        //
        // Why the flush is here and not in `main.rs`'s `hand_off_after_turn` — corrected from
        // what this comment said through REQ-592's implementation. The claim was
        // that a flush hung there would drop the tail of every failed turn. That
        // overstates it: every *arm* of the turn match writes through
        // `Surface::line` after `call` returns, and `line()` emits the held
        // buffer ahead of its own row (BR-8), so on those paths the same bytes
        // land in the same order with or without this call — which is why
        // deleting it leaves the pty suite green.
        //
        // Two things the hand-off still could not do. It **cannot clear the
        // fence bit**, the half only this verb performs: just the `Ok` arm
        // reaches the hand-off, so a turn the daemon refused mid-fence would
        // leave the bit set and render every later reply of the session
        // verbatim. And it never runs at all when `call` returns through its
        // own transport `?` — that error leaves the entry loop without writing
        // a line, so nothing else would emit the tail the drop interrupted.
        //
        // The body is a separate function so that holds on **every** return: the
        // `?` on the send, on each `recv`, on a result that fails to deserialize,
        // and on a dispatch that could not answer a permission are early returns,
        // and a call written at the bottom of the loop would miss all of them.
        //
        // REQ-621 ADR-621-4 hangs the activity row's close-out on this same
        // branch, and for the same reason: it is the one place that runs on
        // every way a turn can end. The row's state is **owned here** and lent
        // to the pump, which is what makes that true of the pump's own early
        // returns as well — the `?` on the send, on each receive, on a result
        // that fails to deserialize, on a dispatch that could not answer a
        // permission, and on the "connection to the daemon closed" a
        // disconnected channel produces. A `RowState` returned by value would
        // be lost on all five, and BR-12 is precisely a rule about the paths
        // nobody anticipated.
        //
        // REQ-622 ADR-622-1 hangs the *terminal's mode* on the same value, for
        // the same reason a third time: the guard that puts canonical mode back
        // is a field of `row`, so it too survives every one of those five early
        // returns, and a panic unwinding through the pump as well (BR-7, AC-5).
        // Which calls may ask for it is the **method's** answer and is asked
        // here — the one place in this client that knows both whether the RPC
        // is a turn and whether there is a terminal on each end of it.
        let mut row = RowState::new(ctx.surface);
        row.engage_input(ctx, P::ENDS_TURN);
        let outcome = self.pump_until_answered(params, ctx, &mut row);
        // BR-12, **before either flush and on both branches**. The row's own
        // visibility is the guard, so this is a no-op wherever there is no row
        // — which is every non-turn RPC, since only a turn can open the
        // projection. Unconditional anyway, because the alternative is a
        // reachability argument ("a non-turn call cannot be in flight during a
        // turn", true today by this client's synchrony) standing between the
        // rule and the code, and BR-12 is a rule about the paths nobody
        // anticipated. ADR-621-4 puts the close-out on the `ENDS_TURN` branch;
        // the withdraw is hoisted out of it and the summary is not.
        //
        // `withdraw_row_above` leaves the cursor on the row it cleared, so
        // anything either flush still has held lands where the row was rather
        // than one line below a blank gap — and a turn's scrollback ends up
        // byte-identical to what it would have been without the row at all
        // (BR-5). `row` dies at the end of this function, so there is nothing
        // left to mark as gone.
        //
        // The report is BR-13's and there is nothing left here to do with it:
        // the call is returning, `row` dies at the end of this function, and a
        // verbose notice about rows that are already out of scope would be news
        // about nothing. A write that failed while the turn was running has
        // already said so, at the site that could still act on it.
        //
        // REQ-622 ADR-622-4: **both** rows, and the block's own visibility is
        // still the whole guard — `withdraw_rows` withdraws what is there,
        // bottom row first, and does nothing when there is nothing.
        let _ = row.withdraw_rows(ctx);
        // And then the terminal itself, in that order (BR-7): the rows come
        // down while the mode the pump painted them in is still in force, and
        // everything below this line — the turn's closing lines, the hand-off,
        // the cost summary — is written in the canonical mode the entry prompt
        // that follows will read in. `release_raw` says why this is explicit
        // rather than left to the drop a few lines later.
        row.release_raw();
        ctx.state.input.set_owned(false);
        if P::ENDS_TURN {
            ctx.surface.end_block();
            // BR-16's figures, read from the accumulator the frames read, at
            // the one seam every exit passes through. `finish` leaves the
            // activity idle, which is what makes it safe here: a turn that
            // ended by a path this branch did not expect still reports its own
            // figures once and lends nothing to the next turn (ADR-621-2).
            ctx.state.last_turn_summary = Some(ctx.state.activity.finish(Instant::now()));
        } else {
            // A non-turn RPC still pumped events while it waited, so it may well
            // have left a fragment held — nothing may be held across a return
            // that hands the terminal back to a caller which is about to write
            // through something other than this surface. Flush it, and leave the
            // fence exactly where the streaming turn left it.
            ctx.surface.emit_held();
        }
        outcome
    }

    /// [`Self::call`]'s body: everything up to the answer, with no flushing.
    ///
    /// Also the activity row's one owner while a call is in flight (REQ-621
    /// ADR-621-3). `row` is lent by [`Self::call`] rather than held here so the
    /// close-out survives this function's early returns — see the comment on
    /// that branch.
    fn pump_until_answered<P: RpcMethod>(
        &mut self,
        params: P,
        ctx: &mut UiContext,
        row: &mut RowState,
    ) -> anyhow::Result<Result<P::Result, RpcError>> {
        let id = self.send(params)?;
        loop {
            // ADR-621-1's gate, and the reason BR-6 holds by construction: with
            // no live rows this is the blocking receive the pump has always
            // had, so the piped path never reaches the tick arm below and
            // cannot emit a byte it did not emit before. Nothing about the
            // *method* is consulted — a non-turn RPC pumping through here ticks
            // too, and finds the projection idle with nothing to draw.
            let wake = if row.live {
                self.recv_timeout(FRAME_INTERVAL)?
            } else {
                Wake::Message(self.recv()?)
            };
            // **REQ-622 BR-2/BR-3, ahead of everything that reads or writes the
            // rows.** On a wake that owns the input, the bytes waiting on stdin
            // are taken now: before the frame is computed, so a keystroke and
            // the row that shows it are one paint (the pending row would
            // otherwise always be one tick behind the key that changed it), and
            // before the withdraw below, so the count an Enter just changed is
            // on the activity row that comes back after the durable line.
            //
            // A no-op on every other path — `read_keys` returns on
            // `owns_input`, which is false for a piped session, a non-turn RPC
            // and a terminal that refused the mode change.
            row.read_keys(ctx);
            // **REQ-621 BR-9, and now only where this call did not take the
            // input.** A line submitted while the row was animating has moved
            // the cursor a row down, so every offset the row owns is short by
            // one and the next paint would land on the line the user typed. The
            // row is abandoned rather than repainted or withdrawn — the
            // reasoning is written out at [`RowState::abandon`], including why
            // the frame it leaves in scrollback is the cheaper failure.
            //
            // With `owns_input` there is no such cursor move to miss: Enter is a
            // byte the loop above just read, the kernel echoed nothing, and the
            // pending row comes down under the block's own bookkeeping. That is
            // what retires REQ-621's recorded exception (BR-4), and the guard is
            // the reason `line_waiting` is still here at all — a terminal that
            // refused raw mode is in exactly last month's world.
            //
            // Here rather than in each arm below, because it is not only the
            // paints that would be wrong: the withdraw-before-dispatch and
            // `call`'s own close-out would each erase that line on their way
            // out, and this runs before both on every wake, including the one
            // that carries the response.
            if !row.owns_input() && row.live && ctx.typed_input && (row.line_waiting)() {
                row.abandon();
            }
            let message = match wake {
                // The daemon said nothing for a frame. Advance the animation
                // *after* the paint, so a row's first frame is the cycle's
                // first frame and a redraw between ticks shows the frame the
                // last tick drew.
                Wake::Tick => {
                    row.paint_rows(ctx, Instant::now());
                    row.tick = row.tick.wrapping_add(1);
                    // AC-5's panic leg, and nothing else can reach it: a debug
                    // build, `TETON_TEST_SEAMS=1`, `TETON_TEST_PANIC_MID_TURN=1`
                    // and a turn that actually took the terminal. One tick in,
                    // so the terminal is raw and a row is on screen — the state
                    // the signal-free restore path has to survive
                    // (`panic_mid_turn_armed`).
                    if row.owns_input() && row.tick == 1 && panic_mid_turn_armed() {
                        panic!(
                            "TETON_TEST_PANIC_MID_TURN: panicking one tick into a raw-mode turn"
                        );
                    }
                    continue;
                }
                Wake::Message(message) => message,
            };
            // **Withdraw before anything else writes** — the block's whole
            // discipline (ADR-621-3, extended to two rows by ADR-622-4), and it
            // is here rather than inside `dispatch_event` on purpose: the idle
            // drain shares that function and must never touch rows it does not
            // own. A durable line — `shell: … [running]`, a notice, a
            // permission question — therefore always prints where the block
            // was, and the block comes back beneath it a few lines below.
            //
            // A streamed token is not a durable line. What the renderer is
            // holding stays held across the withdraw and the redraw — the
            // block's verbs hold, `Surface::draw_row` — so a reply streamed
            // past a pending row reaches the screen in the rows it would have
            // had with no block at all (REQ-622 BR-4).
            if !row.withdraw_rows(ctx) {
                row.hide_after_a_failed_write(ctx);
            }
            match message {
                Incoming::Response(resp) => {
                    match route_response(&id, &resp.id, resp.error.is_some()) {
                        RespRoute::Match => {
                            return Ok(match resp.error {
                                Some(err) => Err(err),
                                None => {
                                    Ok(serde_json::from_value(resp.result.unwrap_or(Value::Null))
                                        .map_err(|e| stale_daemon_hint(P::METHOD, &e))?)
                                }
                            });
                        }
                        // REQ-544 minor: an uncorrelatable `Id::Null` parse-error
                        // frame belongs to this — the only — in-flight request;
                        // surface it rather than looping forever for a numeric-id
                        // reply the daemon can never send.
                        RespRoute::Surface => {
                            // `route_response` only surfaces a frame it saw an
                            // error on, so `error` is `Some` here — but this runs
                            // on the production event pump, so a malformed frame
                            // must fail the call, not panic the CLI.
                            let err = resp.error.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "daemon returned an uncorrelatable error frame with no \
                                     error payload"
                                )
                            })?;
                            return Ok(Err(err));
                        }
                        RespRoute::Ignore => {} // stray ack (e.g. a permission reply)
                    }
                }
                // Nothing to tear down mid-turn — the entry frame came down
                // before the prompt was sent — so the render hook is empty here.
                Incoming::Event(env) => {
                    // **Folded before it is rendered** (ADR-621-3). The render
                    // arms and the row must describe the same moment: a
                    // `tool_call` that painted its `[running]` line while the
                    // projection still said `awaiting_model` would put a row
                    // naming the *previous* phase directly beneath the line
                    // announcing the new one.
                    let arrived = Instant::now();
                    ctx.state.observe_activity(&env, arrived);
                    let dispatched = self.dispatch_event(&env, ctx, &mut || {})?;
                    // BR-1: the row stepped aside for the question, and the
                    // question has been answered — so the phase it interrupted
                    // comes back. Timed from *now* rather than from `arrived`
                    // because a permission prompt sits on screen for as long as
                    // the user takes, and that wait is not the tool running.
                    //
                    // Asked of the dispatch rather than derived from the phase:
                    // an event arriving while the phase is already
                    // `awaiting_permission` — a cost row, a lifecycle stage —
                    // would satisfy any phase test and restore a question
                    // nobody has answered yet.
                    if dispatched.answered_permission {
                        ctx.state.activity.permission_answered(Instant::now());
                    }
                    row.paint_rows(ctx, Instant::now());
                }
                Incoming::Lagged(err) => {
                    report_lag(&err, ctx.surface);
                    row.paint_rows(ctx, Instant::now());
                }
            }
        }
    }

    /// Render every event already queued, without blocking (REQ-556 BR-1).
    ///
    /// The interactive entry loop calls this between polls of stdin, which is
    /// what makes a lifecycle event reach the user at the time it arrives
    /// rather than at the next turn. Before this existed, the only thing that
    /// drained the channel was [`Self::call`]'s pump, so events queued silently
    /// while the loop sat in `read_line` — the daemon knew the tier was ready
    /// and the session had no way to say so.
    ///
    /// Rendering goes through the same [`Self::dispatch_event`] the turn pump
    /// uses, so there is exactly one renderer for any given event (REQ-556
    /// BR-10) and permission/proposal answering behaves identically whether an
    /// event arrives mid-turn or while idle.
    ///
    /// Returns the `model_lifecycle` payloads seen, in order, so an idle caller
    /// can fold them into its indicator without re-inspecting the envelopes.
    ///
    /// `on_first` runs **once**, immediately before the first thing renders,
    /// and only if something is going to render. An interactive caller uses it
    /// to tear down its open entry frame, which would otherwise be overwritten
    /// by the notice. Nothing queued ⇒ `on_first` never runs ⇒ no teardown and
    /// no redraw, so an idle session does not flicker once per poll interval.
    ///
    /// # Errors
    ///
    /// Propagates a send failure from answering a permission or proposal. A
    /// disconnected channel is **not** an error here — it is reported as "no
    /// more events", and the caller discovers the drop on its next `call`.
    pub fn drain_events(
        &mut self,
        ctx: &mut UiContext,
        on_first: impl FnMut(),
    ) -> anyhow::Result<Drained> {
        // REQ-592 BR-8: the idle path. Fragments do reach the surface with no
        // turn in flight — a second client driving the same session broadcasts
        // its stream to this one — and `call`'s flush is no help there, because
        // `call` is not running. Wrapped for the same reason `call` is: the `?`
        // on a permission answer is an early return.
        //
        // **`emit_held`, never `end_block`.** This is a *poll* boundary, not a
        // turn boundary: `main.rs`'s entry loop calls this every
        // `FRAME_INTERVAL` — roughly eight times a second — and it has no way to
        // know whether the broadcasting client's turn is over, because nothing
        // on the bus says so. Ending the block here would clear the fence bit
        // eight times a second, so a broadcast ` ```rust ` block would be
        // reclassified as prose from the very next poll and word-wrapped at the
        // terminal width. That is BR-6's failure arriving through the verb meant
        // to prevent it, and it is why ADR-4's third call site was dropped.
        //
        // Held rows must still go out *inside* this call. `main.rs:798-808`
        // erases the entry frame before the drain and redraws it after, so this
        // is the only window in which a row can be written cleanly — and between
        // windows the loading indicator's `repaint_row_above` would emit the
        // buffer itself (BR-8), scrolling rows into a frame that is on screen.
        // Nothing may be held across this return.
        //
        // The recorded cost: a partial line still on the wire is emitted as a
        // finished row, and a still-growing table run is still closed and
        // re-measured, once per poll. That is the same BR-8 trade `line()` makes
        // at any mid-turn interruption, applied at the poll rate, and removing
        // it needs the frame ownership in `main.rs` to change — not this seam.
        //
        // The other recorded cost, which is this one's mirror image. A client
        // that only *watches* a co-driven session — it never prompts, so it
        // never makes a turn `call` — can be left with the fence bit set
        // **forever** if the broadcasting client's reply ends inside an
        // unterminated ` ``` `. Nothing on the bus says a turn is over
        // (`teton-protocol`'s `Event` has no turn-complete variant, checked),
        // and a turn-ending `call` is the only thing that drops the bit, so
        // every later reply this client sees renders verbatim. Clearing it here
        // is exactly the fix that is worse than the defect — see the paragraph
        // above. The real repair is a turn-boundary event, which is a protocol
        // change and deliberately not invented here.
        //
        // Inert on the common case. An idle poll that drained nothing leaves the
        // buffers empty, so this costs a poll interval nothing.
        let outcome = self.pump_queued(ctx, on_first);
        ctx.surface.emit_held();
        outcome
    }

    /// [`Self::drain_events`]'s body: everything queued, with no flushing.
    fn pump_queued(
        &mut self,
        ctx: &mut UiContext,
        mut on_first: impl FnMut(),
    ) -> anyhow::Result<Drained> {
        let mut drained = Drained::default();
        loop {
            let incoming = match self.incoming.try_recv() {
                Ok(incoming) => incoming,
                // Empty: nothing queued right now. Disconnected: the daemon is
                // gone, which the next `call` reports properly — draining is not
                // the place to fail a session.
                Err(_) => return Ok(drained),
            };
            match incoming {
                Incoming::Event(env) => {
                    // The teardown rides `before_render` rather than happening
                    // here, so an envelope the own-session filter drops leaves
                    // the caller's frame standing: it painted nothing, so there
                    // is nothing to redraw (REQ-568 AC-8).
                    let is_first = drained.rendered == 0;
                    let dispatched = self.dispatch_event(&env, ctx, &mut || {
                        if is_first {
                            on_first();
                        }
                    })?;
                    if dispatched.rendered {
                        drained.rendered += 1;
                    }
                }
                Incoming::Lagged(err) => {
                    if drained.rendered == 0 {
                        on_first();
                    }
                    drained.rendered += 1;
                    report_lag(&err, ctx.surface);
                }
                // A stray ack for a permission or proposal reply we already
                // sent fire-and-forget. `call` ignores these too. Renders
                // nothing, so it must not trigger `on_first`.
                Incoming::Response(_) => {}
            }
        }
    }

    /// Render one event and, if it is a permission request, resolve it and send
    /// the reply back (the ack returns later as a stray response and is ignored).
    ///
    /// Both pumps funnel through here, so this is the one place the own-session
    /// rule has to hold and the one place it is written.
    ///
    /// `before_render` runs immediately before anything paints, and only if
    /// something is going to — an envelope the filter drops never reaches it.
    /// Returns whether the envelope rendered, so a caller that counts paints
    /// counts the ones that happened, and whether this client answered a
    /// permission question — see [`Dispatched`].
    fn dispatch_event(
        &mut self,
        env: &teton_protocol::events::EventEnvelope,
        ctx: &mut UiContext,
        before_render: &mut dyn FnMut(),
    ) -> anyhow::Result<Dispatched> {
        // REQ-568 AC-8: defense in depth atop the daemon-side filter (BR-3),
        // never a substitute for it — the daemon decides who may *see* a
        // session's events, and it is the only place that decision is a control.
        // This drop only keeps a stale or second daemon from painting somebody
        // else's session onto this screen.
        if !should_render(env.session_id.as_ref(), ctx.session_id.as_ref()) {
            return Ok(Dispatched::default());
        }
        before_render();
        let mut answered_permission = false;
        match session_ui::render_event(env, &mut *ctx.surface, &mut *ctx.state) {
            EventOutcome::Rendered => {}
            EventOutcome::Permission(req) if ctx.answer_permissions => {
                // REQ-592 ADR-4, structurally rather than incidentally. The
                // property — **a permission question never paints above
                // assistant text the reader has not been shown** — matters
                // because `prompt.rs` writes questions straight to stdout: a
                // `Prompter` is not a `Surface` and cannot know a buffer is
                // pending, so a held sentence would be printed *after* the user
                // had already answered the question it explains.
                //
                // It held before this line existed, but only by accident:
                // `resolve_permission` happens to render through
                // `surface.line(...)` on all five of its paths before it reaches
                // `prompter.ask`, and `line()` emits the held buffer ahead of its
                // own row (BR-8). That is a property of another module's
                // control flow, one refactor away from being untrue, and it was
                // the *stronger* of REQ-592's two ordering rules — while the
                // weaker one (`end_block`'s ownership) had a source sweep and
                // this had nothing. So the pump states it itself, at the seam it
                // owns, immediately before it hands the terminal to a writer
                // that is not a `Surface`.
                //
                // **`emit_held`, not `end_block`.** A permission prompt is a
                // *pause* in a turn, not the end of one. Clearing the fence bit
                // here is exactly the bug ADR-3's site 3 was dropped for: a model
                // that opens a ```rust fence, hits a tool call, and resumes after
                // the answer would have the rest of its code classified as
                // markdown and word-wrapped at the terminal width, and a wrapped
                // shell command is a different command (BR-6). The fence ends at
                // a turn boundary, and `call` is the turn boundary.
                ctx.surface.emit_held();
                //
                // REQ-622 BR-5's seam, on the arm where type-ahead did the most
                // damage: a line typed while a tool ran sat in the kernel's
                // buffer, and this prompt was the next thing to read stdin, so
                // it was answered by a keystroke meant for the next prompt and
                // the user never saw the question. `around_a_question` puts the
                // pending line aside before the prompter is called and brings it
                // back after.
                let reply = around_a_question(ctx, |ctx| {
                    session_ui::resolve_permission(
                        &req,
                        &mut *ctx.surface,
                        &mut *ctx.prompter,
                        &mut ctx.state.grants,
                        // REQ-585 BR-11 / ADR-8. The terminal fact is threaded
                        // from the one edge that read it (`main.rs`'s
                        // `IsTerminal` on stdin), never recomputed inside the
                        // UI: a handler reading `std::io::stdin()` itself would
                        // be a second, invisible seam, and the gate this feeds
                        // is precisely the one that must not be answerable
                        // differently in two places.
                        ctx.typed_input,
                    )
                });
                self.send(reply)?;
                // REQ-621 BR-1: this is the one arm that knows the question is
                // over, so it is the one arm that says so.
                answered_permission = true;
            }
            EventOutcome::Permission(req) => {
                // Not our session to answer — surface it and leave it to the
                // interactive client that owns it.
                ctx.surface.line(
                    LineKind::Notice,
                    &format!(
                        "permission requested for tool `{}` in another session",
                        req.tool_name
                    ),
                );
            }
            EventOutcome::ModelProposal(proposal) if ctx.answer_model_proposals => {
                if ctx.state.claim_model_proposal(&proposal.request_id) {
                    // ADR-4, the same rule as the permission arm above and for
                    // the same reason: `resolve_proposal` reaches a `Prompter`,
                    // which writes straight to stdout and cannot emit this
                    // surface's held buffer. It happens to render the proposal
                    // through `surface.line` first, which would flush it — but
                    // that is the callee's rendering order, not a property of
                    // this seam, and one reordering inside `model_ui` away from
                    // painting a question above text the reader never saw.
                    // `emit_held`, not `end_block`: a proposal is a pause.
                    ctx.surface.emit_held();
                    // BR-5, the permission arm's rule on the arm that asks the
                    // longest question: a proposal the user reads for a while is
                    // the most likely of the three to have something typed
                    // underneath it.
                    let answered = around_a_question(ctx, |ctx| {
                        model_ui::resolve_proposal(
                            &proposal,
                            ctx.auto_accept_model,
                            &mut *ctx.surface,
                            &mut *ctx.prompter,
                        )
                    });
                    if let Some(reply) = answered {
                        // Fire-and-forget, exactly like a permission answer: the
                        // ack returns later as a stray response and is ignored.
                        // Awaiting it here would re-enter the event pump from
                        // inside an event dispatch.
                        self.send(reply)?;
                    }
                }
            }
            EventOutcome::ModelProposal(proposal) => {
                // Not our prompt to answer, but very much worth seeing: this is
                // why the local tier is unavailable (BR-1/BR-2).
                firstrun::render_proposal(&proposal, &mut *ctx.surface);
                ctx.surface.line(
                    LineKind::Notice,
                    "answer this prompt from an interactive `teton` session.",
                );
            }
            EventOutcome::AttachConsent(request) if ctx.answer_permissions => {
                // REQ-570 BR-4 / AC-4. Gated on `answer_permissions` — the same
                // flag that decides whether this client owns the interactive
                // surface — and deliberately **not** on `auto_accept_model`:
                // `--yes` is consent to this user's own download, never standing
                // authority to admit a different connection into their session.
                //
                // Fire-and-forget like a permission answer: the ack comes back
                // later as a stray response and is ignored. Awaiting it here
                // would re-enter the event pump from inside an event dispatch.
                //
                // ADR-4, and this is the arm where it bites hardest. The
                // question is an access-control consent — "allow this client to
                // watch EVERY session on this daemon?" — and it fires exactly
                // when a second client attaches mid-turn, which is the moment a
                // fragment is most likely to be held. A consent question painted
                // above the text it interrupted is a security prompt the reader
                // answers without its context. `resolve_attach_consent` does
                // write a notice first, and `session_ui.rs` says so in a doc
                // comment — but a doc comment in the callee is not this seam
                // owning its own rule. `emit_held`, not `end_block`: a consent
                // question is a pause in whatever was streaming, not its end.
                ctx.surface.emit_held();
                // BR-5, and here it is an access-control decision as well as a
                // question: a consent prompt answered by a keystroke the user
                // typed for something else is a peer admitted to every session
                // on the daemon by a line meant for the model.
                let reply = around_a_question(ctx, |ctx| {
                    session_ui::resolve_attach_consent(
                        &request,
                        &mut *ctx.surface,
                        &mut *ctx.prompter,
                    )
                });
                self.send(reply)?;
            }
            EventOutcome::AttachConsent(request) => {
                // Not our prompt to answer. Rendered rather than swallowed for
                // `attach_consent_requested`'s original reason: a silent
                // security prompt is worse than an unanswerable one — the user
                // would otherwise see only that a peer quietly failed.
                ctx.surface.line(
                    LineKind::Notice,
                    &session_ui::format_attach_consent_notice(&request),
                );
            }
        }
        Ok(Dispatched {
            rendered: true,
            answered_permission,
        })
    }

    /// Find and answer a proposal that was raised before this client attached.
    ///
    /// The daemon broadcasts `model_selection_proposed` exactly once, never
    /// replays it, and runs the consent flow on a task spawned *beside* the
    /// server (D-3) — so it can publish the proposal before the socket accepts
    /// anyone. A client that waited only for the event would wait forever.
    /// `model/status` is therefore the delivery path, not a fallback: it carries
    /// the entire proposal, so what is rendered here is what the event would have
    /// rendered, named pick and all (BR-2).
    ///
    /// Failures are deliberately quiet — an older daemon without the methods, or
    /// a status call that errors, must not stop a session from starting; the
    /// local tier simply stays gated (BR-1).
    ///
    /// # Errors
    ///
    /// Returns an error only if the connection drops.
    pub fn answer_outstanding_model_proposal(&mut self, ctx: &mut UiContext) -> anyhow::Result<()> {
        if !ctx.answer_model_proposals {
            return Ok(());
        }
        let Ok(status) = self.call(methods::ModelStatusParams::default(), ctx)? else {
            return Ok(());
        };
        let Some(proposal) = status.pending_proposal else {
            return Ok(());
        };
        // The live event may have arrived first (the event pump inside the
        // `model/status` call above could even have delivered it). A proposal is
        // prompted exactly once, and the shared `request_id` is what says so.
        if !ctx.state.claim_model_proposal(&proposal.request_id) {
            return Ok(());
        }
        let reply = model_ui::resolve_outstanding(
            &proposal,
            ctx.auto_accept_model,
            &mut *ctx.surface,
            &mut *ctx.prompter,
        );
        if let Some(reply) = reply {
            // Not inside an event dispatch here, so the answer is sent as a real
            // call: a refusal (an unknown name, or a missing second confirmation)
            // leaves the proposal open and deserves to be shown, not swallowed.
            match self.call(reply, ctx)? {
                Err(err) => ctx.surface.line(
                    LineKind::Error,
                    &format!("the daemon refused the model choice: {}", err.message),
                ),
                // E-8: the daemon accepted the call but found no proposal waiting
                // on that id — it was already answered, or a `teton model set`
                // superseded and cancelled it. Reporting that as success would
                // tell the user their answer decided something when a different
                // decision is on record.
                Ok(result) if !result.delivered => ctx.surface.line(
                    LineKind::Notice,
                    "that proposal was no longer open, so this answer decided nothing — \
                     the decision on record was made elsewhere (`teton model status` shows it).",
                ),
                Ok(_) => {}
            }
        }
        Ok(())
    }

    /// Refresh [`UiContext::skills`] from the daemon (REQ-585 ADR-2).
    ///
    /// Called after `session/create` and again after every
    /// `session_root_changed` — the two moments at which the answer can differ,
    /// because half of it is derived from the session root and `/cd` moves it.
    ///
    /// **This call is the version handshake.** A daemon that does not serve
    /// `skills/list` leaves the snapshot empty and raises no error, which is the
    /// whole mechanism: an old daemon therefore classifies no skills, so
    /// `PromptTurnParams.skill` is never sent to it and the new consent can
    /// never arrive from it. The capability is proven by a successful call, not
    /// asserted from a version number, which is why `PROTOCOL_VERSION` does not
    /// move for any of REQ-585's additions.
    ///
    /// A context with no session — a passive one, or the window before
    /// `session/create` answers — is left empty without a call: there is no
    /// session whose registry could be asked for.
    ///
    /// # Errors
    ///
    /// Only if the connection drops. A daemon that *answers* — with a result or
    /// with any error — leaves a session running; a registry nobody could read
    /// is a session with no skills, never a session that fails to start.
    pub fn refresh_skills(&mut self, ctx: &mut UiContext) -> anyhow::Result<()> {
        let Some(session_id) = ctx.session_id.clone() else {
            ctx.skills = crate::slash::SkillSnapshot::empty();
            return Ok(());
        };
        let reply = self.call(methods::SkillsListParams { session_id }, ctx)?;
        ctx.skills = snapshot_from_skills_reply(reply);
        Ok(())
    }

    /// Serialize and write one request; returns the id assigned to it.
    fn send<P: RpcMethod>(&mut self, params: P) -> anyhow::Result<Id> {
        let id = Id::Number(self.next_id);
        self.next_id += 1;
        let request = methods::request(id.clone(), params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(id)
    }

    /// Block for the next incoming message.
    fn recv(&self) -> anyhow::Result<Incoming> {
        self.incoming
            .recv()
            .map_err(|_| anyhow!("connection to the daemon closed"))
    }

    /// [`Self::recv`] with a deadline: the next message, or [`Wake::Tick`] when
    /// `timeout` passed with the daemon silent (REQ-621 ADR-621-1).
    ///
    /// **One call, no crate, no second thread, no change to the reader.** The
    /// channel is a `std::sync::mpsc` receiver and `recv_timeout` is in the
    /// standard library, so making the *wait* interruptible rather than the read
    /// is the whole of BR-4's mechanism — the same shape REQ-556 chose for the
    /// entry loop, which polls stdin with the same interval (ADR-556-1).
    ///
    /// A queued message is returned **immediately**: `recv_timeout` consults the
    /// channel before it consults the clock, so the tick arm adds nothing to the
    /// latency of receiving or rendering an event (BR-9). There is deliberately
    /// no `try_recv` ahead of this call — it would be a second read of the same
    /// fact, and the fast path is already the fast path.
    ///
    /// `Disconnected` is mapped to the sentence [`Self::recv`] produces, from
    /// the same fact: the reader thread is gone. That is what makes BR-12's
    /// disconnect path fall out of the existing error return rather than needing
    /// a case of its own — the pump's `?` is the exit, and `call`'s close-out
    /// runs on it (ADR-621-4).
    fn recv_timeout(&self, timeout: Duration) -> anyhow::Result<Wake> {
        // The scripted double's hold — see [`TimedReceive`]. Nothing here in a
        // non-test build: the field itself does not exist.
        #[cfg(test)]
        if let Some(left) = self.timed.owed.get().checked_sub(1) {
            self.timed.owed.set(left);
            self.timed.ticks.set(self.timed.ticks.get() + 1);
            return Ok(Wake::Tick);
        }
        match self.incoming.recv_timeout(timeout) {
            Ok(message) => {
                // The double's delay is per reply, so the next one is held again.
                #[cfg(test)]
                self.timed.owed.set(self.timed.delay_ticks.get());
                Ok(Wake::Message(message))
            }
            Err(RecvTimeoutError::Timeout) => {
                #[cfg(test)]
                self.timed.ticks.set(self.timed.ticks.get() + 1);
                Ok(Wake::Tick)
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("connection to the daemon closed")),
        }
    }

    /// Hold every reply this connection answers with for `ticks` frames first
    /// (REQ-621 TASK-411). Test-only — see [`TimedReceive`].
    #[cfg(test)]
    pub(crate) fn delay_replies_by_ticks(&self, ticks: u64) {
        self.timed.delay_ticks.set(ticks);
        self.timed.owed.set(ticks);
    }

    /// How many [`Wake::Tick`]s [`Self::recv_timeout`] has reported.
    ///
    /// Zero is the assertion AC-3 rests on: the piped path never entered the
    /// tick arm, rather than entering it and finding nothing to draw.
    #[cfg(test)]
    pub(crate) fn ticks_observed(&self) -> u64 {
        self.timed.ticks.get()
    }

    /// A `Connection` with no daemon behind it, answering the *n*th request it
    /// is sent with the *n*th scripted result (REQ-582 TASK-169).
    ///
    /// The command modules test a handler by *running* it — that is the only way
    /// to assert what a row sends and what it renders in one place — and a
    /// handler needs a connection. This is that connection: a `UnixStream::pair`
    /// gives the writer half a real, connected socket, so every request the
    /// handler makes is readable off `peer` (which is how a test asserts the
    /// method name, or that nothing was sent at all), and the responses come off
    /// the same [`mpsc`] channel the reader thread would have fed. No server, no
    /// thread, no terminal.
    ///
    /// The ids are assigned the way [`Self::send`] assigns them — from 1, in
    /// order — because [`Self::call`] correlates on them. A connection scripted
    /// with fewer results than the handler makes calls reports "connection to
    /// the daemon closed" on the extra call, which is the honest failure: the
    /// test asked for a daemon that stops answering.
    ///
    /// No handshake happened, so `daemon_name`/`daemon_version` are `None` —
    /// what `/doctor`'s session arm renders from an unnegotiated connection is
    /// its documented fallback, not an invention.
    #[cfg(test)]
    pub(crate) fn scripted(results: &[Value]) -> (Self, UnixStream) {
        Self::scripted_replies(results.iter().cloned().map(Ok).collect())
    }

    /// [`Self::scripted`] for a daemon that may answer a request with an
    /// [`RpcError`] (REQ-582 verify, T1/M4).
    ///
    /// The success-only fixture cannot reach a handler's error arms — a
    /// `config/set` the daemon refused, a `config/get` on a build too old to
    /// serve it — and those arms are where a stored key is taken back out of the
    /// keychain (BUG-171) and where `/doctor` says "not exposed" rather than
    /// failing. Each reply is `Ok(result)` or `Err(error)`, matched to the *n*th
    /// request exactly as [`Self::scripted`] matches its results.
    #[cfg(test)]
    pub(crate) fn scripted_replies(replies: Vec<Result<Value, RpcError>>) -> (Self, UnixStream) {
        let (conn, tx, peer) = paired_for_test();
        for (index, reply) in replies.into_iter().enumerate() {
            let id = Id::Number(i64::try_from(index).expect("a test scripts few responses") + 1);
            let response = match reply {
                Ok(result) => Response::success(id, result),
                Err(error) => Response::failure(id, error),
            };
            tx.send(Incoming::Response(response))
                .expect("the receiver is alive: it is inside the connection");
        }
        (conn, peer)
    }

    /// Assert that every scripted reply was consumed by a request (REQ-582
    /// verify, T4).
    ///
    /// A fixture scripted with *more* replies than the handler made calls would
    /// pass every assertion about what was sent while the test's picture of the
    /// exchange — "then it asked for X, which the daemon answered with Y" — was
    /// one call longer than the truth. The scripting sender is dropped when the
    /// fixture is built, so an empty channel reads as disconnected here and a
    /// leftover reply reads as `Ok`. Call it at the end of a test that scripted
    /// anything.
    ///
    /// # Panics
    ///
    /// When a scripted reply is still queued.
    #[cfg(test)]
    pub(crate) fn assert_all_consumed(&self) {
        assert!(
            self.incoming.try_recv().is_err(),
            "a scripted reply was never consumed: the handler made fewer calls than the test \
             scripted answers for"
        );
    }
}

/// Every JSON-RPC request a handler wrote to a scripted connection's socket, in
/// order, as parsed frames (REQ-582 TASK-169; shared since the verify pass).
///
/// Non-blocking, so "nothing was sent" is an assertion a test can make without
/// waiting for a daemon that does not exist. `peer` is the other end of the
/// [`Connection::scripted`] pair. Frames rather than method names because the
/// keychain tests read the `params` — a registration must carry
/// `keychain://…` and never the key — and a second reader of the same bytes in
/// each module would be two fixtures for one socket.
#[cfg(test)]
pub(crate) fn requests_written(peer: &UnixStream) -> Vec<Value> {
    use std::io::Read;
    peer.set_nonblocking(true).expect("nonblocking");
    let mut raw = Vec::new();
    // WouldBlock is the expected end of the stream here; whatever was read
    // before it is in the buffer.
    let _ = (&mut { peer }).read_to_end(&mut raw);
    String::from_utf8_lossy(&raw)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a framed JSON-RPC request"))
        .collect()
}

/// The method names of [`requests_written`], in order.
#[cfg(test)]
pub(crate) fn methods_written(peer: &UnixStream) -> Vec<String> {
    requests_written(peer)
        .iter()
        .map(|request| {
            request["method"]
                .as_str()
                .expect("every request names a method")
                .to_owned()
        })
        .collect()
}

/// The scripted connection's control over [`Connection::recv_timeout`]: how
/// many ticks to make the pump wait for each reply, and how many ticks it has
/// actually reported (REQ-621 TASK-411).
///
/// **Why a seam in the transport rather than a sleeping thread.** BR-1's
/// contract is an exact sequence — a row drawn, repainted twice, withdrawn —
/// and the only way to hold a reply for *exactly* three frames is to hold it
/// deliberately. A test that queued the reply from a thread after
/// `3 × FRAME_INTERVAL` would assert "about three" and would spend a third of a
/// second doing it; one that raced the send against the pump's next wait could
/// tick four times and go red on a busy machine. Held here, the whole thing is
/// deterministic and costs no wall clock at all.
///
/// The tally is not only for the animation tests. AC-3's guarantee is that the
/// piped path *never enters the tick arm*, and the honest way to assert an arm
/// was not taken is to count the arm's own entries and find zero — a recording
/// with no activity row in it would also pass against a pump that woke eight
/// times a second and simply had nothing to draw.
///
/// The same trade the `@delay-ms` script directive makes on the daemon's side
/// (ADR-621-6): a fixture needs a way to hold a turn open, and inventing a
/// production delay to make a test possible would be the wrong one.
///
/// **It lives down here, not beside [`Connection`], for a reason worth
/// keeping.** `status::scan::production_sources` truncates each file at its
/// first *column-zero* `#[cfg(test)]`, so a test-only item declared above the
/// production code silently shrinks what every source sweep over this file can
/// see — `only_the_event_pump_declares_a_block_over` counted zero `end_block`
/// call sites when this sat under the struct, which is the sweep going blind
/// rather than the rule being broken. Indented `#[cfg(test)]` attributes (the
/// field on `Connection`, the two accessors, the hold in `recv_timeout`) are
/// invisible to that search and stay where they belong.
#[cfg(test)]
#[derive(Debug, Default)]
struct TimedReceive {
    /// How many ticks to report before *each* reply is handed over. Re-armed
    /// as each message goes out, so a script of three events with a delay of
    /// one holds the pump for one frame before each of them — the shape a real
    /// turn has, rather than one long silence at the front.
    delay_ticks: std::cell::Cell<u64>,
    /// How much of the current delay is still owed.
    owed: std::cell::Cell<u64>,
    /// Every tick this connection has reported, held or real.
    ticks: std::cell::Cell<u64>,
}

// The width `RowState::at_width` installs, so a test can resize a terminal it
// does not have (REQ-621, verify).
//
// Down here with `TimedReceive` and for its reason: a column-zero
// `#[cfg(test)]` above the production code truncates what every source sweep
// over this file can see. (A `//` comment rather than a doc comment because
// rustdoc does not document what a macro invocation produces.)
#[cfg(test)]
thread_local! {
    static TEST_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(80) };
}

/// The test terminal's current width — [`RowState::measure_width`]'s answer
/// under `cfg(test)`.
#[cfg(test)]
fn test_width() -> usize {
    TEST_WIDTH.with(std::cell::Cell::get)
}

/// Script what [`RowState::engage_input`] will find at the terminal (REQ-622).
///
/// Down here with the rest of the scripted transport, and for its reason: the
/// *hooks* have to be compiled in every build (`RowState::new`), but a setter
/// only a test calls does not.
#[cfg(test)]
fn script_raw_mode(verdict: InputVerdict) {
    SCRIPTED_VERDICT.with(|slot| slot.set(verdict));
    SCRIPTED_ENGAGE_CALLS.with(|calls| calls.set(0));
}

/// How many times the pump has asked the terminal for its input.
///
/// AC-7's assertion, and the honest shape of it: "no `tcsetattr` was called" is
/// a claim about a call that did not happen, and the only way to assert one is
/// to count the calls that did.
#[cfg(test)]
fn engage_attempts() -> usize {
    SCRIPTED_ENGAGE_CALLS.with(std::cell::Cell::get)
}

/// Script the keystrokes the pump will find, one burst per look at stdin.
///
/// Each burst is followed by an empty one, which is the "and then nothing" of a
/// real look: `read_available` polls, reads once, and the pump's loop asks
/// again and finds the descriptor empty. Without the terminator a single look
/// would drain the whole script, and a test meaning "one word per tick" would
/// silently be testing "every word on the first tick".
#[cfg(test)]
fn script_keystrokes(bursts: &[&[u8]]) {
    SCRIPTED_KEYS.with(|keys| {
        let mut keys = keys.borrow_mut();
        keys.clear();
        for burst in bursts {
            keys.push_back(burst.to_vec());
            keys.push_back(Vec::new());
        }
    });
}

/// Resize the test terminal. The next row **drawn** is fitted to `width`; a row
/// already on screen keeps the width it was drawn at, which is the asymmetry
/// [`RowState::width`] describes and the one a resize test is about.
#[cfg(test)]
fn set_test_width(width: usize) {
    TEST_WIDTH.with(|cell| cell.set(width));
}

/// The socket-pair fixture both test constructors are built on.
///
/// Separate from [`Connection::scripted`] because the event tests in this module
/// need the raw [`Sender`] to push [`Incoming::Event`]s, and [`Incoming`] is this
/// module's own type — handing it out crate-wide to save these five lines would
/// export the transport's internals for a test's convenience.
#[cfg(test)]
fn paired_for_test() -> (Connection, Sender<Incoming>, UnixStream) {
    let (writer, peer) = UnixStream::pair().expect("socketpair");
    let (tx, rx) = mpsc::channel();
    (
        Connection {
            writer,
            incoming: rx,
            next_id: 1,
            // No handshake happened on this fixture, so there is genuinely no
            // daemon version or name to report (REQ-565, REQ-582).
            daemon_version: None,
            daemon_name: None,
            timed: TimedReceive::default(),
        },
        tx,
        peer,
    )
}

/// How a received [`Response`] correlates against the single in-flight request a
/// caller is awaiting (REQ-544 minor).
#[derive(Debug, PartialEq, Eq)]
enum RespRoute {
    /// The correlated reply for the pending id — resolve the call.
    Match,
    /// An uncorrelatable `Id::Null` error frame (a parse error the daemon could
    /// not attribute to an id). Because the synchronous client has exactly one
    /// request in flight, it belongs to that request — surface its error and end
    /// the wait, so the caller does not stall forever awaiting a numeric-id reply
    /// that will never come.
    Surface,
    /// A stray/uncorrelated frame (a different id, or a non-error null id) — skip.
    Ignore,
}

/// Decide how a response frame with id `resp_id` (carrying an error iff
/// `has_error`) routes for a caller awaiting `pending`. Pure so the null-id
/// anti-stall rule is unit-testable without a live socket (REQ-544 minor).
fn route_response(pending: &Id, resp_id: &Id, has_error: bool) -> RespRoute {
    if resp_id == pending {
        RespRoute::Match
    } else if *resp_id == Id::Null && has_error {
        RespRoute::Surface
    } else {
        RespRoute::Ignore
    }
}

/// Whether this client paints an envelope scoped to `envelope_session`, given
/// that it owns `ours` (REQ-568 AC-8, ADR-E). Pure, like [`route_response`],
/// so the rule is readable on its own.
///
/// `pub(crate)` since REQ-621's verify pass: the activity row is a projection
/// of what this session *rendered*, so [`crate::activity::TurnActivity::observe`]
/// folds on this same predicate rather than on a second one that agrees today
/// (BR-15).
///
/// Daemon-scoped envelopes (`None` — model download progress, daemon lifetime)
/// always render, including while `ours` is still `None`: the window before
/// `session/create` answers is exactly where first-run consent speaks, and a
/// client that went quiet there would download 18 GiB in silence. A
/// session-scoped envelope renders only when it names our session, so a client
/// without one renders nothing session-scoped — none of it is its own.
pub(crate) fn should_render(
    envelope_session: Option<&teton_protocol::SessionId>,
    ours: Option<&teton_protocol::SessionId>,
) -> bool {
    match envelope_session {
        None => true,
        Some(sid) => ours == Some(sid),
    }
}

/// The command that replaces a running daemon with the upgraded binary.
///
/// `brew services restart` covers the managed install, which is how the README
/// tells every user to upgrade. An unmanaged daemon (a direct `teton-code`, a
/// dev build) has no service to restart, so the parenthetical names the manual
/// equivalent — `teton` autostarts a fresh one from beside its own binary, which
/// is by construction the build the user just installed.
const RESTART_REMEDY: &str = "restart it with `brew services restart teton` (or stop the running \
                              `teton-code` — `teton` starts a fresh one on the next command)";
/// The command that replaces a stale CLI.
const UPGRADE_REMEDY: &str = "upgrade it with `brew upgrade teton`";

/// Turn a rejected handshake into a sentence with an action in it.
///
/// The version-skew arm is the whole reason this function exists. The socket and
/// lock filenames are stable across releases (ADR-007), so `brew upgrade teton`
/// without the matching `brew services restart` leaves a new CLI talking to a
/// daemon from the previous release — the single commonest way these two
/// binaries disagree, and one the user resolves in one command *if* anybody
/// tells them which command.
///
/// Anything else is passed through untouched: inventing a remedy for an error
/// this function does not understand is how a user ends up restarting a daemon
/// over a problem the restart cannot fix.
fn explain_handshake_failure(err: RpcError) -> anyhow::Error {
    if err.code != error_code::UNSUPPORTED_PROTOCOL_VERSION {
        return anyhow::Error::new(err);
    }
    let Some(mismatch) = handshake::VersionMismatch::from_rpc_error(&err) else {
        // The code says "version", but the bounds did not survive. Name both
        // remedies in likelihood order rather than guessing at one.
        return anyhow!(
            "the daemon refused this client's protocol version ({}). Most often the daemon is \
             an older build still running after an upgrade — {RESTART_REMEDY}. If it persists, \
             this CLI is the older half — {UPGRADE_REMEDY}.",
            err.message
        );
    };
    let client = handshake::format_range(mismatch.client_min, mismatch.client_max);
    let daemon = handshake::format_range(mismatch.daemon_min, mismatch.daemon_max);
    match mismatch.skew() {
        // No daemon *build* version to quote: it only ever arrives in a
        // successful handshake, and this is the failed one. The protocol number
        // is the whole of what the rejection carries.
        Some(handshake::VersionSkew::DaemonIsOlder) => anyhow!(
            "the running daemon speaks protocol {daemon} but this CLI speaks {client}, so they \
             share no version and no command can be served. An upgrade replaces the binaries on \
             disk without restarting a daemon that is already running — {RESTART_REMEDY}."
        ),
        Some(handshake::VersionSkew::ClientIsOlder) => anyhow!(
            "this CLI speaks protocol {client} but the running daemon speaks {daemon}, so they \
             share no version and no command can be served. The CLI is the older half here — \
             {UPGRADE_REMEDY}."
        ),
        // Disjoint enough for the daemon to refuse, yet not classifiable —
        // a malformed advertisement. Report it as the version problem it is
        // without prescribing a fix that may not apply.
        None => anyhow!(
            "the daemon refused this client's protocol version: it speaks {daemon} and this CLI \
             offered {client}. Check that both binaries come from the same release."
        ),
    }
}

/// Explain a daemon reply this build's protocol types could not read.
///
/// The version handshake is the real gate, and once both halves are on a build
/// that has it, this is unreachable for a *released* skew. It stays as the
/// backstop for the case the handshake cannot catch: two builds that agree on
/// the protocol version while a shape changed underneath it — exactly the
/// mistake that produced this bug, whose only symptom was a bare
/// `missing field \`category\`` with no hint that a daemon restart would fix it.
fn stale_daemon_hint(method: &str, err: &serde_json::Error) -> anyhow::Error {
    anyhow!(
        "the daemon's reply to `{method}` does not match this build's protocol types ({err}). \
         That means the two binaries disagree about a message shape: the likeliest cause is a \
         daemon left running from a previous release — {RESTART_REMEDY}."
    )
}

/// Render a subscription-lag eviction as a visible error line.
fn report_lag(err: &RpcError, surface: &mut dyn Surface) {
    surface.line(
        LineKind::Error,
        &format!("event stream lagged and was reset: {}", err.message),
    );
}

/// The reader thread: parse newline-delimited frames and classify each.
fn reader_loop(stream: UnixStream, tx: &Sender<Incoming>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF: daemon closed the connection
            Ok(_) => {}
            Err(_) => break, // read error: tear the reader down
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(incoming) = classify(trimmed) {
            if tx.send(incoming).is_err() {
                break; // the Connection was dropped
            }
        }
    }
}

/// Classify one raw JSON-RPC frame into an [`Incoming`], or `None` if it is not
/// something the client acts on.
fn classify(raw: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(raw).ok()?;
    match value.get("method").and_then(Value::as_str) {
        Some(EVENT_METHOD) => {
            let params = value.get("params")?.clone();
            let envelope = serde_json::from_value(params).ok()?;
            Some(Incoming::Event(Box::new(envelope)))
        }
        Some(SUBSCRIPTION_LAGGED_METHOD) => {
            let params = value.get("params")?.clone();
            let err = serde_json::from_value(params).ok()?;
            Some(Incoming::Lagged(err))
        }
        Some(_) => None, // an unknown notification method; ignore
        None => {
            let resp = serde_json::from_value::<Response<Value>>(value).ok()?;
            Some(Incoming::Response(resp))
        }
    }
}

/// Connect to the daemon, autostarting `teton-code` if the socket is absent.
///
/// # Errors
///
/// Returns an error if the daemon cannot be reached even after autostart, or if
/// the handshake is rejected.
pub fn ensure_connected(
    paths: &DaemonPaths,
    surface: &mut dyn Surface,
) -> anyhow::Result<Connection> {
    // REQ-565 BR-3, client side. A daemon that has committed to exiting refuses
    // the handshake rather than accepting a session it will not serve, and that
    // refusal is **retryable** — the remedy is a successor, not an error. Every
    // other handshake failure still propagates: a protocol mismatch cannot be
    // fixed by starting another daemon from the same binary, and swallowing it
    // into a spawn-retry would spin while hiding the one diagnosis that matters.
    match connect_and_handshake(&paths.socket) {
        Ok(Some(conn)) => {
            report_build_skew(&conn, surface);
            return Ok(conn);
        }
        // Unreachable, or reached-but-shutting-down: both mean "autostart a
        // fresh one", which is what the rest of this function does.
        Ok(None) => {}
        Err(err) => return Err(err),
    }

    surface.line(LineKind::Info, "no daemon reachable — starting teton-code…");
    spawn_daemon(&paths.log)?;

    if let Some(conn) = poll_for_daemon(paths)? {
        surface.line(LineKind::Info, "daemon started.");
        report_build_skew(&conn, surface);
        return Ok(conn);
    }
    // H-1 (E-4): the daemon we just spawned had no terminal, so whatever it said
    // on the way down went to its log and nowhere else. The commonest cause by
    // far is a config it refused to load — and that refusal is worthless if the
    // user only ever sees "could not reach the daemon". Quote it.
    match tail_daemon_log(&paths.log) {
        Some(tail) => bail!(
            "could not reach the daemon after autostart. The daemon reported:\n{tail}\n\
             (full log: {})",
            paths.log.display()
        ),
        None => bail!(
            "could not reach the daemon after autostart, and it left no diagnostic at {}; \
             try running `teton-code` manually to see why.",
            paths.log.display()
        ),
    }
}

/// The interactive-session variant of [`ensure_connected`]: before falling
/// back to a direct (unmanaged) spawn, give a brew-installed `teton` the
/// chance to register the launchd service instead — the consent-first absorb
/// of `brew services start teton` (see [`crate::service`]). Every gate that
/// makes the offer inapplicable (non-macOS, piped stdin, dev build, recorded
/// decline) falls straight through to the plain path, byte-identical.
pub fn ensure_connected_session(
    paths: &DaemonPaths,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> anyhow::Result<Connection> {
    match connect_and_handshake(&paths.socket) {
        Ok(Some(conn)) => {
            report_build_skew(&conn, surface);
            return Ok(conn);
        }
        Ok(None) => {}
        Err(err) => return Err(err),
    }
    if crate::service::offer_registration(paths, surface, prompter) {
        if let Some(conn) = poll_for_daemon(paths)? {
            surface.line(
                LineKind::Info,
                "daemon registered with launchd and started — it will keep running when you \
                 exit, and survive reboots.",
            );
            report_build_skew(&conn, surface);
            return Ok(conn);
        }
        // launchd accepted the service but no socket appeared in time. The
        // direct path below spawns its own daemon (a second instance exits 0
        // with "already running", so a slow service start races harmlessly)
        // and owns the log-quoting diagnostics for a daemon that dies on boot.
        surface.line(
            LineKind::Notice,
            "the service was registered but the daemon has not answered yet — trying a direct \
             start.",
        );
    }
    ensure_connected(paths, surface)
}

/// Connect and handshake, distinguishing "no usable daemon here" from a real
/// failure (REQ-565).
///
/// - `Ok(Some(conn))` — attached.
/// - `Ok(None)` — nothing to attach to *yet*: either the socket is absent, or a
///   daemon answered but is shutting down. Both are resolved by a successor, so
///   the caller autostarts or keeps polling.
/// - `Err(_)` — a real failure the user must see, protocol mismatch above all.
fn connect_and_handshake(socket: &Path) -> anyhow::Result<Option<Connection>> {
    let Ok(mut conn) = Connection::connect(socket) else {
        return Ok(None);
    };
    match conn.handshake() {
        Ok(_) => Ok(Some(conn)),
        Err(err) if is_shutting_down(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Whether a handshake failure is the daemon saying "I am on my way out".
fn is_shutting_down(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RpcError>()
        .is_some_and(|rpc| rpc.code == error_code::DAEMON_SHUTTING_DOWN)
}

/// Poll the socket until a daemon answers the handshake, or give up after
/// [`POLL_ATTEMPTS`]. `Ok(None)` is "nobody came up", not an error — the
/// caller owns the diagnostics for its own start path.
fn poll_for_daemon(paths: &DaemonPaths) -> anyhow::Result<Option<Connection>> {
    for _ in 0..POLL_ATTEMPTS {
        thread::sleep(POLL_INTERVAL);
        // A predecessor mid-teardown can still have the socket bound for a
        // moment and will refuse the handshake. That is "keep polling", not
        // "give up" — the successor we spawned is waiting on the same flock
        // (see `tetond::single_instance::acquire_within`), so the next attempt
        // reaches it.
        if let Some(conn) = connect_and_handshake(&paths.socket)? {
            return Ok(Some(conn));
        }
    }
    Ok(None)
}

/// The build-skew sentence, or `None` when the two halves agree (BR-6/AC-7).
///
/// The check the *protocol* negotiation cannot make: two adjacent releases
/// almost always speak the same protocol version, so a v0.1.12 daemon still
/// serving after v0.1.13 was installed handshakes cleanly and says nothing —
/// the exact harm REQ-565 was written for. The handshake has already succeeded
/// by the time this runs, so it is a notice, never an error.
///
/// Pure and version-injected, so AC-7 is provable without a daemon, a socket, or
/// a second build on disk. The *classification* lives in `teton-protocol`, which
/// is transport-free and knows nothing about how a client is installed; the
/// remedy sentence lives here, where that is known.
fn build_skew_line(
    client_version: &str,
    daemon_version: &str,
    lifetime: DaemonLifetime,
) -> Option<String> {
    let skew = handshake::build_skew(client_version, daemon_version)?;
    let remedy = match lifetime {
        DaemonLifetime::OnDemand => {
            "Exit every teton session to stop it; the next one starts the new daemon."
        }
        // BUG-174: an always-on daemon is running under `--shutdown-policy
        // never`, so closing sessions is precisely the thing that cannot reach
        // it. Naming `brew services stop` is the difference between a notice
        // the user can act on and one that repeats forever.
        DaemonLifetime::AlwaysOnService => {
            "This is the always-on `brew services` daemon, which does not exit with your last \
             session — run `brew services stop teton` once, and the next command starts the new \
             daemon on demand."
        }
    };
    Some(format!(
        "this CLI is {} but the running daemon is {} — commands are being served by the older \
         binary. {remedy}",
        skew.client_version, skew.daemon_version
    ))
}

/// How the daemon now serving us was started — the fact that decides which
/// remedy can actually end it (BUG-174).
///
/// This is not cosmetic. The two lifetimes are stopped by disjoint actions, and
/// the remedy for one is a no-op against the other: a user on an always-on
/// daemon who is told to close their sessions will close them, see the same
/// notice, and conclude the tool is broken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DaemonLifetime {
    /// Started on demand by a CLI. It exits with its last client (REQ-565), so
    /// ending every session is sufficient and sufficient advice.
    OnDemand,
    /// Registered with `brew services`. The formula runs it under
    /// `--shutdown-policy never` (REQ-565 BR-5), so it *by design* does not
    /// exit with its last client, and only `brew services stop` ends it.
    AlwaysOnService,
}

/// Render [`build_skew_line`] for a freshly attached connection.
fn report_build_skew(conn: &Connection, surface: &mut dyn Surface) {
    let Some(daemon_version) = conn.daemon_version() else {
        return;
    };
    // Decide there is something to say *before* paying for a `brew` subprocess:
    // skew is the rare case, and this runs on every attach.
    if handshake::build_skew(CLIENT_VERSION, daemon_version).is_none() {
        return;
    }
    let lifetime = if crate::service::brew_reports_service_running() {
        DaemonLifetime::AlwaysOnService
    } else {
        DaemonLifetime::OnDemand
    };
    if let Some(line) = build_skew_line(CLIENT_VERSION, daemon_version, lifetime) {
        surface.line(LineKind::Notice, &line);
    }
}

/// How many bytes of the daemon log to quote back on an autostart failure.
const LOG_TAIL_BYTES: u64 = 4096;

/// The last few lines of the daemon's captured stderr, when it wrote any.
///
/// Bounded: a log that has been appended to across many runs must not be pasted
/// into a terminal in full, and the cause of *this* failure is at the end.
fn tail_daemon_log(log: &Path) -> Option<String> {
    let text = read_tail(log, LOG_TAIL_BYTES)?;
    let tail = text
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    (!tail.trim().is_empty()).then_some(tail)
}

/// Read at most the last `limit` bytes of `path` as lossy UTF-8.
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > limit {
        file.seek(SeekFrom::Start(len - limit)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Spawn a detached `teton-code` daemon process. It takes the single-instance lock itself,
/// so a redundant spawn is harmless (the extra process exits cleanly).
///
/// Its stderr goes to `log` rather than `/dev/null` (E-4). A daemon started this
/// way has no terminal, so discarding stderr discarded every reason it could give
/// for failing to come up — including the config refusal H-1 added, which every
/// existing user carrying REQ-544's hard-deprecated `pinned_local_model` key hits
/// on their first start. Appending (not truncating) keeps the previous run's
/// explanation if this one dies before writing its own.
fn spawn_daemon(log: &Path) -> anyhow::Result<()> {
    let binary = daemon_binary_path();
    Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(daemon_log_sink(log))
        .spawn()
        .map_err(|e| anyhow!("failed to start daemon `{}`: {e}", binary.display()))?;
    Ok(())
}

/// Size past which the daemon log is restarted rather than appended to.
///
/// Appending keeps the previous run's explanation when this one dies before
/// writing its own; a cap keeps that from becoming an unbounded file in the
/// user's state directory. Only the tail is ever read, so nothing of value is
/// lost by starting over.
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// The stderr sink for a spawned daemon: the log file, or `/dev/null` if it
/// cannot be opened (a daemon that cannot log is still a daemon worth starting).
fn daemon_log_sink(log: &Path) -> Stdio {
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let oversized = std::fs::metadata(log).is_ok_and(|meta| meta.len() > LOG_MAX_BYTES);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(oversized)
        .truncate(oversized)
        .open(log)
        .map_or_else(|_| Stdio::null(), Stdio::from)
}

/// Locate the `teton-code` daemon binary: next to this executable if present, else on PATH.
fn daemon_binary_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_daemon_binary(exe_dir.as_deref())
}

/// Pure resolver: prefer `teton-code` beside `exe_dir`, else the bare name for PATH.
fn resolve_daemon_binary(exe_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = exe_dir {
        let candidate = dir.join("teton-code");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("teton-code")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;
    use std::os::unix::net::UnixListener;
    use teton_protocol::events::ModelLifecycleStage;
    use teton_protocol::handshake::HandshakeError;
    use teton_protocol::ProtocolVersion;

    use crate::render::{RecordingSurface, Rendered};

    /// The rejection a daemon of the given range sends a client of this build —
    /// produced by the protocol's own negotiator rather than hand-built, so the
    /// test cannot pass against a `data` payload the daemon never emits.
    fn rejection_from_daemon(daemon_min: u32, daemon_max: u32) -> RpcError {
        teton_protocol::handshake::negotiate(
            ProtocolVersion(daemon_min),
            ProtocolVersion(daemon_max),
            PROTOCOL_VERSION_MIN,
            PROTOCOL_VERSION_MAX,
        )
        .map(|v| panic!("expected a rejection, negotiated {v}"))
        .unwrap_err()
        .to_rpc_error()
    }

    /// The released pairing, in the words the user gets.
    ///
    /// `brew upgrade teton` without `brew services restart teton` leaves a v1
    /// daemon (v0.1.10, the last release) answering a v2 CLI on the socket path
    /// ADR-007 keeps stable. Before the version bump that combination produced
    /// `missing field \`category\`` and nothing else. The bar here is that the
    /// replacement is *actionable*: it says who is stale and names the command.
    #[test]
    fn a_daemon_left_running_across_an_upgrade_is_told_to_restart() {
        let text = explain_handshake_failure(rejection_from_daemon(1, 1)).to_string();

        assert!(
            text.contains("running daemon speaks protocol 1"),
            "must name the daemon's version: {text}"
        );
        assert!(
            text.contains("this CLI speaks 2"),
            "must name the CLI's version: {text}"
        );
        assert!(
            text.contains("brew services restart teton"),
            "the remedy is the point of the message: {text}"
        );
        // The cause is the half users get wrong — an upgrade is two steps.
        assert!(text.contains("without restarting a daemon"), "{text}");
        // Not a serde error, and not a bare error code.
        assert!(!text.contains("missing field"), "{text}");
        assert!(!text.contains("-32000"), "{text}");
    }

    /// The mirror case needs the opposite remedy: telling someone to restart a
    /// daemon that is already newer than their CLI sends them in a circle.
    #[test]
    fn a_stale_cli_is_told_to_upgrade_itself_not_to_restart_the_daemon() {
        let future = PROTOCOL_VERSION_MAX.0 + 3;
        let text = explain_handshake_failure(rejection_from_daemon(future, future)).to_string();

        assert!(text.contains("this CLI speaks protocol 2"), "{text}");
        assert!(text.contains(&format!("daemon speaks {future}")), "{text}");
        assert!(text.contains("brew upgrade teton"), "{text}");
        assert!(
            !text.contains("brew services restart"),
            "the restart remedy does not apply and must not be offered: {text}"
        );
    }

    /// A daemon that reports the version code without decodable bounds still has
    /// to leave the user with something to do — but must not fabricate a range.
    #[test]
    fn a_version_rejection_without_bounds_still_names_both_remedies() {
        let bare = RpcError::new(
            error_code::UNSUPPORTED_PROTOCOL_VERSION,
            "no mutually supported protocol version",
        );
        let text = explain_handshake_failure(bare).to_string();

        assert!(text.contains("brew services restart teton"), "{text}");
        assert!(text.contains("brew upgrade teton"), "{text}");
        // Nothing invented: no version number appears at all.
        assert!(!text.contains("protocol 1"), "{text}");
        assert!(!text.contains("protocol 2"), "{text}");
    }

    /// Every other rejection passes through verbatim. A remedy attached to an
    /// error this code does not understand sends the user to restart a daemon
    /// over a problem a restart cannot fix.
    #[test]
    fn a_non_version_rejection_keeps_its_own_words() {
        let err = RpcError::new(error_code::INVALID_PARAMS, "invalid handshake params");
        let text = explain_handshake_failure(err).to_string();

        assert!(text.contains("invalid handshake params"), "{text}");
        assert!(!text.contains("brew"), "{text}");
    }

    /// The backstop for a shape that drifts *without* a version bump — the
    /// mistake that caused this bug. A user who hits it gets the same diagnosis
    /// as a version skew, because the cause and the fix are the same.
    #[test]
    fn an_unreadable_reply_blames_the_stale_daemon_rather_than_leaking_serde() {
        // The real failure: the v1 routing row, read by the v2 type.
        let err =
            serde_json::from_value::<teton_protocol::methods::ConfigSnapshot>(serde_json::json!({
                "providers": [],
                "routing": [{"phase": "io", "provider_id": "local"}],
                "privacy": []
            }))
            .unwrap_err();
        let text = stale_daemon_hint("config/get", &err).to_string();

        assert!(text.contains("config/get"), "{text}");
        assert!(text.contains("brew services restart teton"), "{text}");
        assert!(
            text.contains("previous release"),
            "must name the cause, not just the symptom: {text}"
        );
        // The serde detail is kept — it is the only clue to *which* shape drifted
        // — but it is no longer the entire message.
        assert!(text.contains("missing field"), "{text}");
        assert!(text.len() > err.to_string().len() * 3, "{text}");
    }

    /// The daemon's own sentence is the fallback for a client too old to decode
    /// `data`, so it must survive a trip through `anyhow` intact.
    #[test]
    fn the_daemons_sentence_reaches_a_client_that_cannot_decode_the_bounds() {
        let err = HandshakeError::IncompatibleVersion {
            client_min: ProtocolVersion(1),
            client_max: ProtocolVersion(1),
            daemon_min: PROTOCOL_VERSION_MIN,
            daemon_max: PROTOCOL_VERSION_MAX,
        };
        let text = err.to_string();
        assert!(text.contains("this client is the older build"), "{text}");
    }

    /// A `Connection` whose channel the test feeds directly. `UnixStream::pair`
    /// gives the writer half a real, connected socket without a daemon, so
    /// `drain_events` is exercised with no server, no terminal, and no thread.
    fn test_connection() -> (Connection, mpsc::Sender<Incoming>, UnixStream) {
        paired_for_test()
    }

    fn lifecycle_envelope(model: &str, stage: ModelLifecycleStage) -> Incoming {
        use teton_protocol::events::{Event, EventEnvelope, ModelLifecycle};
        Incoming::Event(Box::new(EventEnvelope {
            session_id: None,
            seq: 1,
            event: Event::ModelLifecycle(ModelLifecycle {
                model_id: model.to_owned(),
                stage,
            }),
        }))
    }

    /// A session-scoped envelope: `phase_transition` is the very event
    /// REQ-568's multi-client test used to prove one client could read
    /// another's stream, so it is the honest stand-in for "somebody's session
    /// output".
    fn phase_envelope(session: &str) -> Incoming {
        use teton_protocol::events::{Event, EventEnvelope, PhaseTransition};
        Incoming::Event(Box::new(EventEnvelope {
            session_id: Some(teton_protocol::SessionId::from(session)),
            seq: 1,
            event: Event::PhaseTransition(PhaseTransition {
                from_phase: None,
                to_phase: teton_protocol::Phase::Implement,
                artifacts: Vec::new(),
            }),
        }))
    }

    /// REQ-556 BR-1. Before `drain_events`, the only thing that emptied this
    /// channel was `call`'s pump, so a lifecycle event that arrived while the
    /// entry loop sat in `read_line` stayed queued until the next turn — the
    /// daemon knew the tier was ready and the session had no way to say so.
    ///
    /// Exercised with no socket server and no terminal, which is the point:
    /// BR-2 hides the interactive path from every piped e2e, so this behaviour
    /// needs a route that does not involve a tty.
    #[test]
    fn draining_renders_queued_events_and_reports_their_lifecycle_stages() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Verifying {
                total_bytes: 18_600_000_000,
            },
        ))
        .expect("queue");
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Ready,
        ))
        .expect("queue");

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: None,
            skills: crate::slash::SkillSnapshot::empty(),
        };

        let mut teardowns = 0;
        let drained = conn
            .drain_events(&mut ctx, || teardowns += 1)
            .expect("drain");

        assert_eq!(drained.rendered, 2, "both queued events rendered");
        // The caller's frame is torn down once, not once per event — otherwise
        // a burst would erase and redraw for every line in it.
        assert_eq!(teardowns, 1, "on_first runs exactly once per drain");
        assert!(
            surface.any_line_contains(LineKind::Notice, "ready"),
            "the ready line reached the surface: {:?}",
            surface.calls
        );
        // The indicator was folded on the way through, in arrival order, so the
        // terminal `Ready` wins and the session stops animating (REQ-556 BR-6).
        // Asserting through session state rather than a returned vector is the
        // point: this is the path a real session takes.
        assert!(
            state.loading.frame(0).is_none(),
            "a drained Ready must leave the indicator hidden"
        );
    }

    /// The fold happens in `render_event`, so an event drained while idle and
    /// an event drained by a turn's pump reach the indicator identically. This
    /// pins the idle half; the mid-turn half rides the same `dispatch_event`.
    #[test]
    fn draining_a_mid_load_stage_leaves_the_indicator_visible() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Verifying {
                total_bytes: 18_600_000_000,
            },
        ))
        .expect("queue");

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: None,
            skills: crate::slash::SkillSnapshot::empty(),
        };
        conn.drain_events(&mut ctx, || {}).expect("drain");
        assert!(
            state.loading.frame(0).is_some(),
            "a verifying tier is work in progress and must draw"
        );
    }

    /// An idle session must not flicker: with nothing queued, the caller's
    /// entry frame is never torn down, so there is nothing to redraw.
    #[test]
    fn draining_an_empty_channel_touches_nothing() {
        let (mut conn, _tx, _peer) = test_connection();
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: None,
            skills: crate::slash::SkillSnapshot::empty(),
        };

        let mut teardowns = 0;
        let drained = conn
            .drain_events(&mut ctx, || teardowns += 1)
            .expect("drain");

        assert_eq!(drained.rendered, 0);
        assert_eq!(teardowns, 0, "no teardown when nothing renders");
        assert!(
            surface.calls.is_empty(),
            "nothing rendered: {:?}",
            surface.calls
        );
    }

    // -----------------------------------------------------------------------
    // REQ-592 BR-8 / ADR-3: no line falls off the end of a turn (TASK-280)
    // -----------------------------------------------------------------------

    /// A streamed chunk of assistant text, as the daemon broadcasts it.
    fn agent_chunk(text: &str) -> Incoming {
        use teton_protocol::events::{Event, EventEnvelope, SessionUpdate, SessionUpdatePayload};
        Incoming::Event(Box::new(EventEnvelope {
            session_id: None,
            seq: 1,
            event: Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::AgentMessageChunk {
                    text: text.to_owned(),
                },
            }),
        }))
    }

    /// Move the activity into `Streaming`, where it draws **no row at all**
    /// (ADR-621-1: arriving text is its own liveness signal).
    ///
    /// The one state in which the pending row is on screen with nothing above
    /// it, which is what the two tests below are about — the reflow when the
    /// row leaves, and BR-14's count with no row left to carry it. Folded
    /// through `TurnActivity::observe` from the same envelope the pump would
    /// hand it, rather than by setting a phase, so the tests cannot be a claim
    /// about a state the daemon never produces.
    fn the_reply_starts_streaming(ctx: &mut UiContext, now: Instant) {
        let Incoming::Event(envelope) = agent_chunk("the finding is") else {
            unreachable!("agent_chunk builds an event")
        };
        ctx.state
            .activity
            .observe(&envelope, ctx.session_id.as_ref(), now);
    }

    /// The turn request these tests put in flight. The method is the real one —
    /// `session/prompt` is what a typed line sends — so what the pump does here
    /// is what it does to a turn.
    fn turn_params() -> methods::PromptTurnParams {
        methods::PromptTurnParams {
            session_id: teton_protocol::SessionId::from("s1"),
            prompt: Vec::new(),
            skill: None,
        }
    }

    /// A permission request as the daemon raises one, with a single
    /// reject-once option — enough for `resolve_permission` to render, ask, and
    /// compose a reply.
    fn permission_envelope(tool: &str) -> Incoming {
        use teton_protocol::events::{
            Event, EventEnvelope, PermissionOption, PermissionOptionKind, PermissionRequest,
        };
        Incoming::Event(Box::new(EventEnvelope {
            session_id: None,
            seq: 2,
            event: Event::PermissionRequest(PermissionRequest {
                request_id: teton_protocol::RequestId::from("r1"),
                tool_name: tool.to_owned(),
                description: Some("run `cargo test`".to_owned()),
                subject: None,
                options: vec![PermissionOption {
                    option_id: "reject_once".to_owned(),
                    label: "Reject once".to_owned(),
                    kind: PermissionOptionKind::RejectOnce,
                }],
            }),
        }))
    }

    /// The daemon's answer to the first request a fixture sends.
    fn turn_answered(id: i64) -> Incoming {
        Incoming::Response(Response::success(
            Id::Number(id),
            serde_json::json!({ "turn_id": "t1", "stop_reason": "end_turn" }),
        ))
    }

    /// Run `body` with a context over a markdown-rendering surface and return
    /// the bytes that reached it.
    ///
    /// A rendering surface rather than a `RecordingSurface`, because a surface
    /// that buffers nothing cannot show the difference between "emitted" and
    /// "held" — which is the entire subject of these tests.
    fn bytes_from_a_rendering_pump(body: impl FnOnce(&mut UiContext)) -> String {
        let mut buf = Vec::new();
        {
            let mut surface = crate::render::PlainSurface::with_markdown(&mut buf, false, 60);
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id: None,
                skills: crate::slash::SkillSnapshot::empty(),
            };
            body(&mut ctx);
        }
        String::from_utf8(buf).expect("the surface writes utf-8")
    }

    /// **AC-10.** A turn whose final chunk carries no trailing newline still has
    /// its last line on screen by the time `call` returns — which is before
    /// anything in `main.rs` runs, and therefore before the entry frame is
    /// redrawn.
    ///
    /// The mutation this is aimed at: delete the `end_block()` in
    /// [`Connection::call`] and the tail is held forever, so the user's answer
    /// ends one sentence short and the session goes back to the prompt as if it
    /// had said everything.
    #[test]
    fn a_turn_ending_without_a_newline_still_reaches_the_screen() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("the finding is that the guard holds"))
            .expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let out = bytes_from_a_rendering_pump(|ctx| {
            let answered = conn.call(turn_params(), ctx).expect("the response arrives");
            assert!(answered.is_ok(), "the daemon answered this turn");
        });

        assert_eq!(out, "the finding is that the guard holds\n");
    }

    /// **AC-10's failed-turn leg — the path `hand_off_after_turn` never
    /// reaches.**
    ///
    /// At main.rs:1356 only the `Ok` arm of the turn match calls the hand-off,
    /// so a flush hung there would never run on a turn the daemon refused. What
    /// this test pins is that `call` itself returns with nothing held, whichever
    /// answer came back.
    ///
    /// **What it does *not* prove, corrected at REQ-592's verify:** that moving
    /// the flush into the hand-off would lose these bytes on screen. It would
    /// not. `main.rs`'s failure arms all write through `Surface::line`, and
    /// `line()` emits the held buffer ahead of its own row (BR-8), so the same
    /// text lands in the same order. The half that cannot be moved is the fence
    /// clear — see the comment on the call site, and
    /// `render::an_unterminated_fence_does_not_swallow_the_next_turn` for what
    /// happens when it does not run.
    #[test]
    fn a_turn_the_daemon_failed_still_flushes_what_it_had_already_streamed() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("I will check the tier first"))
            .expect("queue");
        tx.send(Incoming::Response(Response::failure(
            Id::Number(1),
            RpcError::new(error_code::INTERNAL_ERROR, "the tier fell over mid-turn"),
        )))
        .expect("queue");

        let out = bytes_from_a_rendering_pump(|ctx| {
            let answered = conn.call(turn_params(), ctx).expect("a frame did arrive");
            assert!(
                answered.is_err(),
                "this fixture's turn must fail: it is the point of the test"
            );
        });

        assert_eq!(out, "I will check the tier first\n");
    }

    /// **AC-10's transport leg.** The `?` on `recv` is an early return that no
    /// call written at the bottom of the loop would cover: the daemon dropped
    /// the connection with a partial line in the buffer, and the user still has
    /// to see what they were told before the drop.
    #[test]
    fn a_turn_whose_connection_dropped_still_flushes_what_it_had_streamed() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("halfway through the answer"))
            .expect("queue");
        // No response, and the only sender goes away — so `recv` fails and
        // `call` returns through its transport `?`.
        drop(tx);

        let out = bytes_from_a_rendering_pump(|ctx| {
            let dropped = conn.call(turn_params(), ctx);
            assert!(dropped.is_err(), "the fixture hangs up on this turn");
        });

        assert_eq!(out, "halfway through the answer\n");
    }

    /// **AC-10's idle leg.** Fragments do arrive with no turn in flight — a
    /// second client driving the same session broadcasts its stream to this one
    /// — and `call`'s flush is no help there, because `call` is not running.
    #[test]
    fn a_fragment_drained_while_idle_is_emitted_not_held() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("a line from the other client"))
            .expect("queue");

        let out = bytes_from_a_rendering_pump(|ctx| {
            let drained = conn.drain_events(ctx, || {}).expect("drain");
            assert_eq!(drained.rendered, 1, "the chunk was dispatched");
        });

        assert_eq!(out, "a line from the other client\n");
    }

    /// **The idle drain is a *poll* boundary, not a turn boundary (REQ-592
    /// verify, MAJOR 1).**
    ///
    /// `main.rs`'s entry loop calls `drain_events` every `FRAME_INTERVAL` —
    /// about eight times a second — and nothing on the event bus says when a
    /// broadcasting client's turn is over. So a drain that ended the block would
    /// clear the fence bit between one poll and the next, and every remaining
    /// line of a ` ```rust ` block arriving from the other client would be
    /// classified as markdown and word-wrapped at the terminal width.
    ///
    /// The fixture is two drains on **one** surface, because a per-drain surface
    /// would carry no fence bit between them and prove nothing. The resumed line
    /// is deliberately longer than the fixture's 60 columns, so a classified copy
    /// is unmistakably re-flowed rather than coincidentally identical.
    ///
    /// (Verified by mutation: restoring `end_block()` at the end of
    /// `drain_events` splits the second row into three.)
    #[test]
    fn an_idle_drain_does_not_end_a_fence_the_other_client_is_still_inside() {
        const RESUMED: &str =
            "let b = *p * *q; // a deliberately long trailing comment that keeps going";
        let (mut conn, tx, _peer) = test_connection();

        let out = bytes_from_a_rendering_pump(|ctx| {
            // Poll one: the broadcast opens a fence and streams a line of code.
            tx.send(agent_chunk("```rust\nlet a = 1;\n"))
                .expect("queue");
            conn.drain_events(ctx, || {}).expect("drain");
            // Poll two, one frame interval later: the same block continues.
            tx.send(agent_chunk(&format!("{RESUMED}\n")))
                .expect("queue");
            conn.drain_events(ctx, || {}).expect("drain");
        });

        assert_eq!(
            out,
            format!("let a = 1;\n{RESUMED}\n"),
            "the fence did not survive the poll, so a broadcast code block was \
             re-flowed as prose (BR-6)"
        );
        assert!(
            !out.contains("```"),
            "the fence marker was printed: {out:?}"
        );
    }

    /// A line inside a fence that is comfortably wider than the fixture's 60
    /// columns, so a copy that was classified as prose is unmistakably re-flowed
    /// rather than coincidentally identical.
    ///
    /// No `*`, `_` or backtick in it on purpose: the pair of tests below read
    /// "the fence was dropped" off the **wrapping**, and inline emphasis eating
    /// a character would make the negative assertion pass for the wrong reason.
    const RESUMED_CODE: &str =
        "let b = q; // a deliberately long trailing comment that keeps going here";

    /// **`/cost` typed mid-stream must not re-flow somebody else's code block
    /// (REQ-592 confirmation review, MAJOR 1).**
    ///
    /// `Connection::call` is the **RPC** boundary. It was treated as the turn
    /// boundary through implementation and verify, and it is not: about thirty
    /// of its callers are slash handlers, setup walkthroughs and status probes.
    /// Dropping the fence on any of those is BR-6's failure — the one the poll
    /// path was cleaned of at verify — arriving instead at the frequency a user
    /// types commands.
    ///
    /// The fixture is a broadcast fence interrupted by one non-turn `call`, on
    /// **one** surface, because a per-call surface would carry no fence bit
    /// across the interruption and prove nothing.
    ///
    /// (Verified by mutation: making the branch unconditional — `end_block()`
    /// on every RPC, which is what shipped — splits the resumed line in two.)
    #[test]
    fn a_non_turn_call_does_not_end_a_fence_the_other_client_is_still_inside() {
        let (mut conn, tx, _peer) = test_connection();

        let out = bytes_from_a_rendering_pump(|ctx| {
            // The other client opens a fence and streams a line of code.
            tx.send(agent_chunk("```rust\nlet a = 1;\n"))
                .expect("queue");
            conn.drain_events(ctx, || {}).expect("drain");
            // This user types `/cost` while that block is still streaming.
            tx.send(Incoming::Response(Response::success(
                Id::Number(1),
                serde_json::to_value(methods::CostQueryResult::default()).expect("a cost report"),
            )))
            .expect("queue");
            let answered = conn
                .call(methods::CostQueryParams::default(), ctx)
                .expect("the response arrives");
            assert!(answered.is_ok(), "the fixture answers the cost query");
            // The same block continues afterwards.
            tx.send(agent_chunk(&format!("{RESUMED_CODE}\n")))
                .expect("queue");
            conn.drain_events(ctx, || {}).expect("drain");
        });

        assert_eq!(
            out,
            format!("let a = 1;\n{RESUMED_CODE}\n"),
            "the fence did not survive a `cost/query`, so a slash command \
             re-flowed a broadcast code block as prose (BR-6)"
        );
    }

    /// The other half of the same gate: `session/prompt` **is** a turn, and its
    /// `call` still drops the fence.
    ///
    /// Without this leg the fix is only half-pinned — `if false` in place of
    /// `if P::ENDS_TURN` would satisfy the test above and leave an unterminated
    /// fence rendering every later reply of the session verbatim, which is the
    /// defect `end_block()` exists for.
    ///
    /// (Verified by mutation: `if false` leaves the resumed line unwrapped and
    /// fails here.)
    #[test]
    fn a_turn_call_does_end_the_fence_the_reply_left_open() {
        let (mut conn, tx, _peer) = test_connection();

        let out = bytes_from_a_rendering_pump(|ctx| {
            tx.send(agent_chunk("```rust\nlet a = 1;\n"))
                .expect("queue");
            tx.send(turn_answered(1)).expect("queue");
            let answered = conn.call(turn_params(), ctx).expect("the response arrives");
            assert!(answered.is_ok(), "the fixture answers the turn");
            // A later reply of the same session, drained while idle.
            tx.send(agent_chunk(&format!("{RESUMED_CODE}\n")))
                .expect("queue");
            conn.drain_events(ctx, || {}).expect("drain");
        });

        assert!(
            !out.contains(RESUMED_CODE),
            "the turn ended inside an unterminated fence and the bit was never \
             dropped, so the session's next reply is still being rendered \
             verbatim: {out:?}"
        );
        assert!(
            out.starts_with("let a = 1;\n"),
            "the fenced line the turn did stream must still be verbatim: {out:?}"
        );
    }

    /// **ADR-4, as a structural property of the pump rather than of
    /// `resolve_permission`'s control flow (REQ-592 verify, MEDIUM).**
    ///
    /// Two things have to be true at once when a permission question goes up
    /// mid-turn, and before this REQ's verify only one of them was owned by
    /// anything:
    ///
    /// 1. **Nothing may be held.** `prompt.rs` writes the question straight to
    ///    stdout — a `Prompter` is not a `Surface` — so a sentence still in the
    ///    buffer would be printed *after* the user had answered the question it
    ///    explains. The pump now emits held rows itself, immediately before it
    ///    hands the terminal over.
    /// 2. **The fence may not be cleared.** A prompt is a pause in a turn, not
    ///    the end of one, so the code the model resumes after the answer must
    ///    still be verbatim. This is why the emit is `emit_held()` and not
    ///    `end_block()` — ADR-3's dropped third call site.
    ///
    /// The fixture holds a partial line *inside* an open fence and interrupts it
    /// with a real permission event, so both properties are read off one byte
    /// sequence.
    ///
    /// (Verified by mutation: `end_block()` in place of `emit_held()` in the
    /// permission arm re-flows the resumed line across three rows. The
    /// complementary mutation — deleting the call outright — is caught by
    /// `only_the_event_pump_declares_a_block_over`'s region check rather than
    /// here, because `resolve_permission` still renders through `line()` first;
    /// that incidental flush is exactly what the sweep exists to stop being the
    /// guarantee.)
    #[test]
    fn a_permission_prompt_emits_held_text_without_ending_the_fence() {
        const RESUMED: &str =
            "let b = *p * *q; // a deliberately long trailing comment that keeps going";
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk(
            "```rust\nlet a = 1;\nlet held = 2; // no newline yet",
        ))
        .expect("queue");
        tx.send(permission_envelope("shell")).expect("queue");
        tx.send(agent_chunk(&format!("\n{RESUMED}\n")))
            .expect("queue");

        let out = bytes_from_a_rendering_pump(|ctx| {
            conn.drain_events(ctx, || {}).expect("drain");
        });

        let held = out
            .find("let held = 2; // no newline yet")
            .unwrap_or_else(|| panic!("the held line never reached the screen: {out:?}"));
        let question = out
            .find("permission requested: shell")
            .unwrap_or_else(|| panic!("the request never reached the screen: {out:?}"));
        assert!(
            held < question,
            "the question painted above text the reader had not been shown: {out:?}"
        );
        assert!(
            out.contains(&format!("{RESUMED}\n")),
            "the resumed line was re-flowed, so the prompt ended the fence: {out:?}"
        );
        assert!(
            !out.contains("```"),
            "the fence marker was printed: {out:?}"
        );
    }

    /// **The idle drain's error leg**, which `call` has three tests for and this
    /// function had none.
    ///
    /// `pump_queued`'s only `?` is the reply to a permission it could not send —
    /// the daemon went away between raising the request and being answered. The
    /// wrapper is what makes the flush hold on that return too, exactly as it
    /// does on `call`'s transport `?`.
    ///
    /// **Honest about what the fixture proves.** Unlike `call`'s failed-turn and
    /// dropped-connection legs, no mutation confined to this file makes this one
    /// red: every path that can reach that `?` renders through `Surface::line`
    /// first, and `line()` emits the buffer itself. What the test pins is the
    /// property — `drain_events` never returns with a row still held, whichever
    /// way it returns — so a future silent error path is a failure here rather
    /// than a sentence lost off the bottom of a redrawn frame.
    #[test]
    fn a_drain_that_could_not_answer_still_flushes_what_it_had_streamed() {
        let (mut conn, tx, peer) = test_connection();
        tx.send(agent_chunk("the tail that was already streamed"))
            .expect("queue");
        tx.send(permission_envelope("shell")).expect("queue");
        // The daemon is gone, so composing the answer succeeds and sending it
        // does not: `dispatch_event`'s `?` is the way out of this drain.
        drop(peer);

        let out = bytes_from_a_rendering_pump(|ctx| {
            let failed = conn.drain_events(ctx, || {});
            assert!(
                failed.is_err(),
                "this fixture's reply must fail to send: it is the point of the test"
            );
        });

        assert!(
            out.starts_with("the tail that was already streamed\n"),
            "the streamed tail was still held when the drain gave up: {out:?}"
        );
    }

    /// **ADR-4.** A permission question raised mid-turn must paint *below* the
    /// assistant text that preceded it, never above it.
    ///
    /// `prompt.rs` writes questions and the entry frame straight to stdout — it
    /// never goes through a `Surface`, which is why it cannot know a buffer is
    /// pending. The fixture reproduces exactly that: the prompter and the
    /// surface share one sink, so "what reached the screen, in order" is a
    /// single byte sequence and the ordering is a property of it rather than of
    /// two separate recordings a test lined up by hand.
    ///
    /// **Where the guarantee comes from, and where it used to come from.**
    /// Through REQ-592's implementation it came from `resolve_permission`
    /// itself: it renders the request through `surface.line(...)` before it ever
    /// reaches `prompter.ask` — on all five of its paths, the two auto-decision
    /// arms and the over-budget offer included — and `line()` emits the pending
    /// buffer ahead of its own row (BR-8). True, but incidental: a property of
    /// another module's control flow, one refactor away from being untrue, and
    /// the only one of this REQ's two ordering rules with no structural owner.
    /// Since verify the pump states it itself, with an `emit_held()` immediately
    /// before that call. **Not** `end_block()` — ADR-3's third call site was
    /// dropped because clearing the fence bit at a mid-turn pause mangles a
    /// fenced block the model resumes after the answer (BR-6), and that reason
    /// is unchanged.
    ///
    /// So this test asserts the **property**, deliberately independent of which
    /// mechanism provides it, and it outlives the call site it was written
    /// beside. A refactor that drops that `line()`, or a new prompt path that
    /// asks before it renders, fails here — instead of silently reordering the
    /// screen at the one moment the order is the point: the question names the
    /// thing the user is being asked to allow, and the sentence explaining it
    /// would be printed after the answer was given.
    #[test]
    fn a_permission_question_paints_below_the_text_that_preceded_it() {
        use std::cell::RefCell;
        use std::rc::Rc;

        /// One sink with two writers, standing in for the terminal.
        #[derive(Clone, Default)]
        struct SharedSink(Rc<RefCell<Vec<u8>>>);

        impl io::Write for SharedSink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        /// A prompter that bypasses the `Surface` exactly as the real one does.
        struct SinkPrompter {
            sink: SharedSink,
        }

        impl Prompter for SinkPrompter {
            fn ask(&mut self, question: &str) -> Option<String> {
                use io::Write;
                let _ = writeln!(self.sink, "{question}");
                Some("n".to_owned())
            }
            fn ask_secret(&mut self, question: &str) -> Option<String> {
                self.ask(question)
            }
        }

        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("let me run the test suite"))
            .expect("queue");
        tx.send(Incoming::Event(Box::new(
            teton_protocol::events::EventEnvelope {
                session_id: None,
                seq: 2,
                event: teton_protocol::events::Event::PermissionRequest(
                    teton_protocol::events::PermissionRequest {
                        request_id: teton_protocol::RequestId::from("r1"),
                        tool_name: "shell".to_owned(),
                        description: Some("run `cargo test`".to_owned()),
                        subject: None,
                        options: vec![teton_protocol::events::PermissionOption {
                            option_id: "reject_once".to_owned(),
                            label: "Reject once".to_owned(),
                            kind: teton_protocol::events::PermissionOptionKind::RejectOnce,
                        }],
                    },
                ),
            },
        )))
        .expect("queue");

        let sink = SharedSink::default();
        {
            let mut surface = crate::render::PlainSurface::with_markdown(sink.clone(), false, 60);
            let mut state = SessionState::new();
            let mut prompter = SinkPrompter { sink: sink.clone() };
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id: None,
                skills: crate::slash::SkillSnapshot::empty(),
            };
            conn.drain_events(&mut ctx, || {}).expect("drain");
        }

        let screen = String::from_utf8(sink.0.borrow().clone()).expect("utf-8");
        let text = screen.find("let me run the test suite").unwrap_or_else(|| {
            panic!("the streamed sentence never reached the screen: {screen:?}")
        });
        let question = screen
            .find("allow shell?")
            .unwrap_or_else(|| panic!("the question never reached the screen: {screen:?}"));
        assert!(
            text < question,
            "the question painted above text the reader had not been shown, so the \
             screen reads in the wrong order (ADR-4): {screen:?}"
        );
    }

    /// **ADR-3's ownership rule, as a check rather than as a sentence**
    /// ([[LESSON-547]]).
    ///
    /// A rule that crosses a seam is owned by exactly one side, and "the turn
    /// loop flushes" plus "the surface flushes" are indistinguishable at review
    /// time from two documents that agree. Scans the whole client, because the
    /// failure this guards against is a *new* call site somewhere else — in
    /// `main.rs` beside `hand_off_after_turn`, or as a self-flush inside
    /// `render.rs` — which is the direction a rule like this actually rots in.
    ///
    /// **Counted *and* region-checked, uniformly.** The confirmation review
    /// found this test applying its own lesson to one site and not the others:
    /// the permission arm was pinned to a span of source, while `drain_events`'
    /// flush was held in place by arithmetic alone and could be relocated
    /// anywhere in this file without failing anything. Every required site now
    /// has to be *where* it has to be, and the counts are kept only for the
    /// thing a region cannot see — a sixth site appearing somewhere new.
    #[test]
    fn only_the_event_pump_declares_a_block_over() {
        let sources = crate::status::scan::production_sources();
        assert!(
            !sources.is_empty(),
            "the client source scan matched nothing — it is no longer looking at \
             anything"
        );

        let mut callers: Vec<&str> = Vec::new();
        for (rel, src) in &sources {
            // `.end_block()` is a *call*; `fn end_block` is the declaration and
            // the implementation, both of which live in `render.rs` by design.
            // `.emit_held()` travels with it: it is the half of the verb a
            // non-pump caller would reach for, and the pump owns both.
            let code = crate::status::scan::code_only(src);
            if (code.contains(".end_block()") || code.contains(".emit_held()"))
                && rel != "client.rs"
            {
                callers.push(rel);
            }
        }
        assert!(
            callers.is_empty(),
            "deciding that a block has ended belongs to `client.rs`'s event pump \
             (ADR-3); these files also call it and would be a second owner of the \
             rule: {callers:?}"
        );

        // Non-vacuity, both halves: the pump really does call it, and `render.rs`
        // really does define it.
        let client = sources
            .iter()
            .find(|(rel, _)| rel == "client.rs")
            .map(|(_, src)| crate::status::scan::code_only(src))
            .expect("client.rs is a production source");
        assert_eq!(
            client.matches(".end_block()").count(),
            1,
            "the block-ending verb has exactly **one** call site, because there is \
             exactly one turn boundary in this client: the `P::ENDS_TURN` branch \
             of `call`. ADR-3 named three. The site before `resolve_permission` \
             was dropped during implementation — it clears the fence bit at a \
             *pause* in a turn — and the site at the end of `drain_events` was \
             dropped at verify for the sharper version of the same reason: \
             `main.rs` polls that function every FRAME_INTERVAL, so it is a poll \
             boundary, and a fence cleared eight times a second re-flows a \
             broadcast code block as prose (BR-6). Both now call `emit_held()`, \
             which leaves block state alone"
        );
        assert_eq!(
            client.matches(".emit_held()").count(),
            5,
            "the flush-only verb has exactly five call sites: the non-turn branch \
             of `call` (an RPC that is not a turn still pumped events and may \
             have left a fragment held), the end of `drain_events` (nothing may \
             be held across a return that hands the terminal back to a frame \
             redraw), and **each of the three arms of `dispatch_event` that reach \
             a `Prompter`** — permission, model proposal, attach consent. A \
             `Prompter` is not a `Surface` and cannot flush what this one is \
             holding (ADR-4). The count went 2 → 5 at the confirmation review, \
             which found the two sibling arms unguarded: they were safe only \
             because their callees happen to render through `surface.line` \
             first, which is the incidental property this sweep exists to stop \
             being the guarantee"
        );

        // **Every required site is region-checked, not counted.** The counts
        // above catch a *sixth* call site appearing; they cannot catch an
        // existing one moving, and that is the mutation the confirmation review
        // named: relocate `drain_events`' flush anywhere else in this file and
        // the arithmetic is still satisfied while the rule is broken. So each
        // site is pinned to the span of source it has to sit in.
        fn region<'a>(client: &'a str, start: &str, end: &str) -> &'a str {
            let at = client
                .find(start)
                .unwrap_or_else(|| panic!("this sweep's anchor is gone from client.rs: {start:?}"));
            let len = client[at..]
                .find(end)
                .unwrap_or_else(|| panic!("{end:?} no longer follows {start:?} in client.rs"));
            &client[at..at + len]
        }

        // ADR-3's seam, both halves of it. `call` clears the fence **only** on
        // the turn branch: it is the RPC boundary, not the turn boundary, and
        // some thirty of its callers (`/cost`, `/model`, `/config`, the setup
        // flows) are not turns at all.
        // The anchor gained `&mut row` when the pump took the activity row on
        // loan from this branch (REQ-621 ADR-621-4). The *region* is what this
        // sweep asserts, so the anchor moves with the call it names; the
        // arithmetic above is untouched.
        let call_tail = region(
            &client,
            "let outcome = self.pump_until_answered(params, ctx, &mut row);",
            "fn pump_until_answered<",
        );
        assert!(
            call_tail.contains("if P::ENDS_TURN {")
                && call_tail.contains("ctx.surface.end_block()"),
            "the fence may be dropped only when the RPC that finished was a turn, \
             and that has to be asked of the **method** (`P::ENDS_TURN`) rather \
             than of the call site — thirty call sites are thirty chances to \
             answer it wrongly. A `/cost` typed while a second client is \
             mid-` ```rust ` block would otherwise re-flow the rest of that block \
             as prose (BR-6)"
        );
        assert!(
            call_tail.contains("ctx.surface.emit_held()"),
            "a non-turn RPC still pumped events while it waited, so it still has \
             to flush before it hands the terminal back — it just may not touch \
             block state"
        );

        // The poll boundary. Pinned by region rather than by arithmetic for the
        // reason the region check exists at all.
        assert!(
            region(
                &client,
                "let outcome = self.pump_queued(ctx, on_first);",
                "fn pump_queued(",
            )
            .contains("ctx.surface.emit_held()"),
            "nothing may be held across `drain_events`' return: `main.rs` erases \
             the entry frame before the drain and redraws it after, so this is \
             the only window in which a row can be written cleanly (BR-8)"
        );

        // ADR-4's ordering property, structurally, on **each** arm that hands
        // the terminal to a `Prompter`. The sweep above says the fence is not
        // cleared at a prompt; this says the buffer *is* emitted there, by this
        // pump, rather than by whatever the callee happens to render first.
        for (arm, callee, what) in [
            (
                "EventOutcome::Permission(req) if ctx.answer_permissions",
                "session_ui::resolve_permission(",
                "a permission question",
            ),
            (
                "EventOutcome::ModelProposal(proposal) if ctx.answer_model_proposals",
                "model_ui::resolve_proposal(",
                "a model proposal",
            ),
            (
                "EventOutcome::AttachConsent(request) if ctx.answer_permissions",
                "session_ui::resolve_attach_consent(",
                "an attach-consent question — an access-control decision, and the \
                 one that fires exactly when a second client attaches mid-turn",
            ),
        ] {
            assert!(
                region(&client, arm, callee).contains("ctx.surface.emit_held()"),
                "{what} must never paint above assistant text the reader has not \
                 been shown (ADR-4). `prompt.rs` writes straight to stdout and \
                 cannot emit a held buffer, so the pump has to do it before it \
                 hands the terminal over — and it must be `emit_held`, not \
                 `end_block`, because a prompt is a pause in a turn and not the \
                 end of one. That `{callee}` happens to render through \
                 `surface.line` first is the callee's ordering, not this seam's \
                 rule"
            );
        }

        let render = sources
            .iter()
            .find(|(rel, _)| rel == "render.rs")
            .map(|(_, src)| crate::status::scan::code_only(src))
            .expect("render.rs is a production source");
        assert!(
            render.contains("fn end_block") && render.contains("fn emit_held"),
            "this assertion is only meaningful while render.rs owns both verbs"
        );
    }

    /// **ADR-621-4's close-out, pinned where it actually lives** — the same
    /// mechanism `only_the_event_pump_declares_a_block_over` uses on the fence,
    /// and for the reason that test states: a rule held in place by arithmetic
    /// can be relocated anywhere in the file while the arithmetic still adds up
    /// ([[LESSON-547]], [[LESSON-568]]).
    ///
    /// **REQ-622 adds two rows to it**, both pinned here for the same reason.
    /// `row.engage_input(ctx, P::ENDS_TURN)` sits above the pump — the terminal
    /// is taken for a turn, and which calls are turns is the *method's* answer
    /// — and `row.release_raw()` sits below the withdraw and above the branch,
    /// which is an ordering rather than a location: the rows come down in the
    /// mode they were painted in, and the turn's closing lines go out in the
    /// canonical mode the entry prompt reads in (BR-7). Both are the kind of
    /// rule a reachability argument can hide: leaving the restore to `row`'s
    /// own drop is *nearly* right and passes every behavioural test in the
    /// suite, because a scripted `RowState` holds no guard to drop.
    ///
    /// The close-out is **two parts in two places**, which is the drift this
    /// pass found in the ADR: the withdraw is hoisted *above* `if P::ENDS_TURN`
    /// and the summary is on the branch. The ADR described one block on the
    /// branch, and the difference is load-bearing in both directions:
    ///
    /// - the **withdraw** is guarded by `row.visible` and by nothing else, so
    ///   it does not need to ask what kind of method just finished. Putting it
    ///   on the `ENDS_TURN` branch would make BR-12 rest on the argument that
    ///   no non-turn call can be in flight during a turn — true today by this
    ///   client's synchrony, and exactly the kind of reachability claim BR-12
    ///   exists to not depend on;
    /// - the **summary** is a turn's figures, so it must ask: a `/cost` that
    ///   called `finish` would report a turn nobody ran and overwrite the last
    ///   real one.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** move the withdraw
    /// onto the branch (`if P::ENDS_TURN { if row.visible { … } …`). **1 red of
    /// 828**, this test — and *nothing else*, which is the whole point of a
    /// region check: every behavioural test in the suite drives either a turn
    /// (where the branch is taken and the withdraw still runs) or a non-turn
    /// call that never draws a row, so the rule can be broken without a single
    /// assertion noticing. Reverted with the same edit.
    ///
    /// The second site is `SessionState::begin_turn`, which closes a turn still
    /// found open when the next prompt goes on the wire — the defensive path for
    /// a turn that ended by a route neither of these two saw. It is asserted
    /// here rather than trusted, because a fallback nobody checks is a fallback
    /// that has already been deleted once.
    #[test]
    fn the_rows_close_out_straddles_the_ends_turn_branch() {
        let sources = crate::status::scan::production_sources();

        fn region<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
            let at = src
                .find(start)
                .unwrap_or_else(|| panic!("this sweep's anchor is gone: {start:?}"));
            let len = src[at..]
                .find(end)
                .unwrap_or_else(|| panic!("{end:?} no longer follows {start:?}"));
            &src[at..at + len]
        }

        let source = |name: &str| {
            sources
                .iter()
                .find(|(rel, _)| rel == name)
                .map(|(_, src)| crate::status::scan::code_only(src))
                .unwrap_or_else(|| panic!("{name} is a production source"))
        };
        let client = source("client.rs");

        // Before the pump runs at all: the terminal's mode, asked of the
        // method. REQ-622 ADR-622-1.
        let before_the_pump = region(
            &client,
            "let mut row = RowState::new(ctx.surface);",
            "let outcome = self.pump_until_answered(params, ctx, &mut row);",
        );
        assert!(
            before_the_pump.contains("row.engage_input(ctx, P::ENDS_TURN)"),
            "raw mode is taken for a **turn**, and which calls are turns is the \
             method's answer (`P::ENDS_TURN`) rather than the call site's — \
             thirty of `call`'s callers are not turns, and a `/cost` that \
             flipped the terminal's mode would be a mode change with no reader \
             behind it (BR-1): {before_the_pump}"
        );

        // Between the pump's return and the branch: the withdraw, guarded by
        // the block's own visibility, and then the terminal.
        let before_the_branch = region(
            &client,
            "let outcome = self.pump_until_answered(params, ctx, &mut row);",
            "if P::ENDS_TURN {",
        );
        assert!(
            before_the_branch.contains("row.withdraw_rows(ctx)"),
            "BR-12's withdraw runs on the way out of **every** call, guarded by \
             the block's own visibility — not by what kind of method just \
             finished. On the `ENDS_TURN` branch it would hold only for as long \
             as no non-turn RPC can be in flight during a turn, which is a \
             property of today's control flow and not a rule: {before_the_branch}"
        );
        // REQ-622 BR-7, as an *ordering*: the rows come down in the mode they
        // were painted in, and the mode is restored before the branch below
        // writes the turn's closing lines.
        let (withdrawn, restored) = (
            before_the_branch
                .find("row.withdraw_rows(ctx)")
                .expect("the withdraw is asserted above"),
            before_the_branch.find("row.release_raw()"),
        );
        assert!(
            restored.is_some_and(|restored| restored > withdrawn),
            "the terminal is put back at the close-out and **after** the rows \
             are withdrawn: restoring first would leave this loop taking its \
             rows back through a terminal whose mode it no longer knows, and \
             leaving it to `row`'s drop would put the turn's closing lines out \
             in raw mode (BR-7): {before_the_branch}"
        );

        // On the branch: the summary, and nothing about the rows or the mode.
        let on_the_branch = region(&client, "if P::ENDS_TURN {", "fn pump_until_answered<");
        assert!(
            on_the_branch.contains("ctx.state.activity.finish(Instant::now())"),
            "BR-16's figures are a *turn's*, so they are read where the method \
             says a turn just ended: {on_the_branch}"
        );
        assert!(
            !on_the_branch.contains("withdraw_row") && !on_the_branch.contains("release_raw"),
            "neither the withdraw nor the restore may drift onto the branch: \
             {on_the_branch}"
        );

        // And the defensive second site, in the one place that opens a turn.
        let session_ui = source("session_ui.rs");
        assert!(
            region(
                &session_ui,
                "pub(crate) fn begin_turn(",
                "pub(crate) fn observe_activity(",
            )
            .contains("self.last_turn_summary = Some(self.activity.finish(now))"),
            "a turn still open when the next prompt goes on the wire ended by a \
             path the close-out did not see; it is closed here rather than \
             overwritten, so its cost and its clock are not lent to the turn \
             about to start (ADR-621-2)"
        );
    }

    /// A dropped daemon is reported by the next `call`, not by a drain — a
    /// disconnected channel here must read as "nothing more queued" rather than
    /// failing a session that is otherwise fine.
    #[test]
    fn draining_a_disconnected_channel_is_not_an_error() {
        let (mut conn, tx, _peer) = test_connection();
        drop(tx);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: None,
            skills: crate::slash::SkillSnapshot::empty(),
        };
        let drained = conn.drain_events(&mut ctx, || {}).expect("not an error");
        assert_eq!(drained.rendered, 0);
    }

    /// REQ-568 AC-8 / ADR-E: the pump paints this client's session and nothing
    /// else's.
    ///
    /// Defense in depth atop the daemon-side filter (BR-3), never a substitute
    /// for it — with that filter in place a foreign envelope should never
    /// arrive at all, and this pins what happens to one that does (a stale or
    /// second daemon). The daemon-scoped rows are the other half of the rule:
    /// model download progress and lifecycle carry no session, and a client
    /// that dropped them before `session/create` answered would sit through
    /// first-run consent in silence.
    ///
    /// Driven through `drain_events` rather than around it, because "reached
    /// `render_event`" is only interesting as "reached the user's screen", and
    /// the teardown count is part of that: a dropped envelope must not pull the
    /// entry frame down for a line it never draws.
    #[test]
    fn the_pump_renders_its_own_session_and_daemon_scope_only() {
        let ours = || Some(teton_protocol::SessionId::from("s1"));
        let ready = || lifecycle_envelope("qwen3-coder-30b-a3b", ModelLifecycleStage::Ready);
        let cases: Vec<(&str, Incoming, Option<teton_protocol::SessionId>, bool)> = vec![
            (
                "our own session renders",
                phase_envelope("s1"),
                ours(),
                true,
            ),
            (
                "another session is dropped before it can paint",
                phase_envelope("s2"),
                ours(),
                false,
            ),
            (
                "daemon-scoped renders alongside a session of our own",
                ready(),
                ours(),
                true,
            ),
            (
                "pre-create: nothing session-scoped is ours yet",
                phase_envelope("s2"),
                None,
                false,
            ),
            (
                "pre-create: daemon-scoped still renders (first-run consent)",
                ready(),
                None,
                true,
            ),
        ];

        for (name, envelope, session_id, renders) in cases {
            let (mut conn, tx, _peer) = test_connection();
            tx.send(envelope).expect("queue");

            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id,
                skills: crate::slash::SkillSnapshot::empty(),
            };

            let mut teardowns = 0;
            let drained = conn
                .drain_events(&mut ctx, || teardowns += 1)
                .expect("drain");

            assert_eq!(
                !surface.calls.is_empty(),
                renders,
                "{name}: surface {:?}",
                surface.calls
            );
            assert_eq!(drained.rendered, usize::from(renders), "{name}: count");
            assert_eq!(teardowns, usize::from(renders), "{name}: teardown");
        }
    }

    #[test]
    fn classify_reads_a_success_response() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"session_id":"s1"}}"#;
        match classify(raw) {
            Some(Incoming::Response(resp)) => {
                assert_eq!(resp.id, Id::Number(1));
                assert!(resp.error.is_none());
            }
            _ => panic!("expected a response"),
        }
    }

    #[test]
    fn classify_reads_an_error_response() {
        let raw = format!(
            r#"{{"jsonrpc":"2.0","id":2,"error":{{"code":{},"message":"method not found"}}}}"#,
            error_code::METHOD_NOT_FOUND
        );
        match classify(&raw) {
            Some(Incoming::Response(resp)) => {
                assert_eq!(resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
            }
            _ => panic!("expected an error response"),
        }
    }

    #[test]
    fn classify_reads_an_event_notification() {
        let raw = r#"{"jsonrpc":"2.0","method":"event","params":{
            "session_id":"s1","seq":3,"event":"route_decided",
            "provider_id":"anthropic","reason":"spec routes to the frontier tier"}}"#;
        match classify(raw) {
            Some(Incoming::Event(env)) => {
                assert_eq!(env.event_name(), "route_decided");
                assert_eq!(env.seq, 3);
            }
            _ => panic!("expected an event"),
        }
    }

    #[test]
    fn classify_reads_a_lag_notice() {
        let raw = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription/lagged","params":{{
            "code":{},"message":"subscription evicted"}}}}"#,
            error_code::SUBSCRIPTION_LAGGED
        );
        match classify(&raw) {
            Some(Incoming::Lagged(err)) => {
                assert_eq!(err.code, error_code::SUBSCRIPTION_LAGGED);
            }
            _ => panic!("expected a lag notice"),
        }
    }

    #[test]
    fn classify_ignores_unknown_notifications_and_junk() {
        assert!(classify(r#"{"jsonrpc":"2.0","method":"mystery","params":{}}"#).is_none());
        assert!(classify("not json at all").is_none());
    }

    #[test]
    fn a_null_id_error_frame_is_surfaced_so_a_caller_never_stalls() {
        // REQ-544 minor: the daemon answers an unparseable request with an
        // `Id::Null` error frame. A synchronous caller awaiting its numeric id must
        // NOT loop forever — the null-id error belongs to the single in-flight
        // request and is surfaced to end the wait.
        let pending = Id::Number(7);
        // The correlated reply matches regardless of whether it carries an error.
        assert_eq!(
            route_response(&pending, &Id::Number(7), false),
            RespRoute::Match
        );
        assert_eq!(
            route_response(&pending, &Id::Number(7), true),
            RespRoute::Match
        );
        // A null-id ERROR frame ends the wait (the anti-stall path).
        assert_eq!(
            route_response(&pending, &Id::Null, true),
            RespRoute::Surface
        );
        // A null-id WITHOUT an error is not actionable (never issued in practice) —
        // ignore rather than surface a non-existent error.
        assert_eq!(
            route_response(&pending, &Id::Null, false),
            RespRoute::Ignore
        );
        // A stray ack for a different numeric id is ignored (e.g. a permission
        // reply), matching the prior behavior.
        assert_eq!(
            route_response(&pending, &Id::Number(99), false),
            RespRoute::Ignore
        );
    }

    /// The stub daemon's deadlock backstop: how long it will block on any single
    /// socket operation before giving up (E-10).
    ///
    /// Both sides of this test block: the client waits for a response only the
    /// stub can send, and the stub waits for a line only the client can send. Any
    /// bug that breaks that lock-step wedges both threads — and because the
    /// client's reader thread holds a dup of the socket, dropping the connection
    /// does not give the stub an EOF either. Untimed, `join()` would then hang
    /// until CI killed the whole job, hiding a real failure behind a job timeout.
    /// Long enough that a loaded runner never trips it; short enough to be a test
    /// failure rather than an outage.
    const STUB_IO_TIMEOUT: Duration = Duration::from_secs(20);

    /// How long the stub waits for *more* traffic after the client has answered.
    ///
    /// The exchange is over at that point, so the backstop above would otherwise
    /// be paid in full on every green run. The window still has to be wide enough
    /// to catch the failure this test is about: a client that prompts twice sends
    /// its second `model/confirm` microseconds after the first, so a duplicate
    /// cannot slip out after the stub stops listening.
    const STUB_IDLE_GRACE: Duration = Duration::from_millis(500);

    /// A stub daemon that hands the *same* proposal to a client twice — once as
    /// the broadcast event, once on the `model/status` it answers next — and
    /// records every request the client sent back.
    ///
    /// Deliberately over a real `UnixStream` with the real framing: the de-dup
    /// this proves lives in the seam between the event pump and the status call,
    /// and a hand-fed `SessionState` would not exercise that seam at all.
    fn serve_a_doubly_delivered_proposal(socket: PathBuf) -> thread::JoinHandle<Vec<Value>> {
        let listener = UnixListener::bind(&socket).expect("bind the stub daemon socket");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a client connects");
            stream
                .set_read_timeout(Some(STUB_IO_TIMEOUT))
                .expect("the stub socket accepts a read timeout");
            stream
                .set_write_timeout(Some(STUB_IO_TIMEOUT))
                .expect("the stub socket accepts a write timeout");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = BufWriter::new(stream);
            let proposal = serde_json::json!({
                "request_id": "model-0",
                "probe": {
                    "total_ram_bytes": 17_179_869_184u64,
                    "free_disk_bytes": 536_870_912_000u64,
                    "gpu_class": "apple_silicon",
                    "chosen_band": "small",
                    "reason": "16 GiB of RAM puts this machine in the small band",
                },
                "proposed": {
                    "entry": {
                        "name": "qwen2.5-coder-3b",
                        "band": "small",
                        "size_bytes": 2_147_483_648u64,
                        "ram_floor_bytes": 5_368_709_120u64,
                        "provenance": {
                            "repo": "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF",
                            "host": "huggingface.co",
                            "revision": "f74adce",
                        },
                    },
                    "required_disk_bytes": 3_221_225_472u64,
                },
                "alternatives": [],
            });
            let mut seen = Vec::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let request: Value = match serde_json::from_str(line.trim()) {
                    Ok(value) => value,
                    Err(_) => break,
                };
                line.clear();
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                seen.push(request);
                if method == methods::ModelStatusParams::METHOD {
                    // The event first — the client is mid-call when it lands, so
                    // it is dispatched by the pump before the status reply.
                    let event = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": EVENT_METHOD,
                        "params": {
                            "seq": 1,
                            "event": "model_selection_proposed",
                            "request_id": proposal["request_id"],
                            "probe": proposal["probe"],
                            "proposed": proposal["proposed"],
                            "alternatives": proposal["alternatives"],
                        },
                    });
                    writeln!(writer, "{event}").unwrap();
                    writer.flush().unwrap();
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "pending_proposal": proposal },
                    });
                    writeln!(writer, "{response}").unwrap();
                } else {
                    let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}});
                    writeln!(writer, "{response}").unwrap();
                }
                writer.flush().unwrap();
                if method == methods::ModelConfirmParams::METHOD {
                    // The client has answered, so the conversation is over and
                    // the long backstop has nothing left to protect. Drop to the
                    // grace window: a duplicate answer would already be on its
                    // way, and a green run should not pay 20 seconds to prove one
                    // never came. (A socket option applies to the socket, so this
                    // reaches the reader's dup of it too.)
                    let _ = writer.get_ref().set_read_timeout(Some(STUB_IDLE_GRACE));
                }
            }
            seen
        })
    }

    /// REQ-547: seeing a proposal twice must ask a human once.
    ///
    /// Both delivery paths are live now — the daemon broadcasts the proposal
    /// *and* serves it from `model/status` — so the client meets it twice
    /// whenever it attaches in time to catch the event. Prompting twice would be
    /// a worse failure than the missed event this fixed: the second prompt would
    /// be answered into a waiter that no longer exists.
    #[test]
    fn a_proposal_seen_as_an_event_and_on_model_status_prompts_exactly_once() {
        let socket = std::env::temp_dir().join(format!("tcl{:x}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let server = serve_a_doubly_delivered_proposal(socket.clone());

        let mut conn = Connection::connect(&socket).expect("connect to the stub daemon");
        let mut surface = crate::render::RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&["y"]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: false,
            answer_model_proposals: true,
            auto_accept_model: false,
            // No command runs under this context; the honest value for a test
            // process is the same check the real edge makes.
            typed_input: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            session_id: None,
            skills: crate::slash::SkillSnapshot::empty(),
        };
        conn.answer_outstanding_model_proposal(&mut ctx)
            .expect("the round-trip completes");
        drop(conn);

        let requests = server.join().expect("the stub daemon thread");
        let _ = std::fs::remove_file(&socket);

        assert_eq!(
            prompter.asked, 1,
            "the user must be asked exactly once, however many times the \
             proposal was delivered"
        );
        let confirms = requests
            .iter()
            .filter(|r| r["method"].as_str() == Some(methods::ModelConfirmParams::METHOD))
            .count();
        assert_eq!(confirms, 1, "exactly one answer reaches the daemon");
        // And the one prompt named the pick, its size, and its RAM floor (BR-2).
        let text = surface.lines_of(crate::render::LineKind::Info).join("\n");
        assert_eq!(
            text.matches("proposed: qwen2.5-coder-3b").count(),
            1,
            "the proposal is rendered once, by name: {text}"
        );
        assert!(text.contains("2.0 GiB download"), "{text}");
        assert!(text.contains("needs 5.0 GiB RAM"), "{text}");
        // Proof that both sightings really happened and the *event* was the one
        // that prompted: the late-attach path prints its own notice, and it is
        // absent because `claim_model_proposal` had already taken this id.
        let notices = surface.lines_of(crate::render::LineKind::Notice).join("\n");
        assert!(
            !notices.contains("before this client attached"),
            "the status sighting must have been suppressed, not re-prompted: {notices}"
        );
    }

    #[test]
    fn resolve_daemon_binary_prefers_a_sibling_then_falls_back_to_path() {
        // Empty/absent dir → bare name for PATH lookup.
        assert_eq!(resolve_daemon_binary(None), PathBuf::from("teton-code"));

        let dir = std::env::temp_dir().join(format!("teton-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            resolve_daemon_binary(Some(&dir)),
            PathBuf::from("teton-code")
        );

        // A sibling `teton-code` file is preferred.
        let sibling = dir.join("teton-code");
        std::fs::write(&sibling, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_daemon_binary(Some(&dir)), sibling);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Build-version skew — REQ-565 BR-6 / AC-7
    // -----------------------------------------------------------------------

    #[test]
    fn matching_builds_say_nothing() {
        assert_eq!(
            build_skew_line("0.1.13", "0.1.13", DaemonLifetime::OnDemand),
            None
        );
    }

    #[test]
    fn a_stale_daemon_produces_one_line_naming_both_versions_and_the_remedy() {
        let line = build_skew_line("0.1.13", "0.1.12", DaemonLifetime::OnDemand)
            .expect("a skew must be reported");
        assert!(line.contains("0.1.13"), "{line}");
        assert!(line.contains("0.1.12"), "{line}");
        // The remedy has to be actionable: under exit-on-last-client, ending
        // every session *is* how the old daemon stops.
        assert!(line.contains("Exit every teton session"), "{line}");
        assert_eq!(line.lines().count(), 1, "exactly one line: {line}");
    }

    /// The v0.1.12/v0.1.13 pairing REQ-565 cites: same protocol, so the
    /// handshake succeeds and the existing protocol-skew check stays silent.
    /// This notice is the only thing that speaks.
    #[test]
    fn the_notice_covers_the_case_the_protocol_check_cannot_see() {
        assert!(
            handshake::negotiate(
                PROTOCOL_VERSION_MIN,
                PROTOCOL_VERSION_MAX,
                PROTOCOL_VERSION_MIN,
                PROTOCOL_VERSION_MAX,
            )
            .is_ok(),
            "the pairing under test must be one that negotiates cleanly"
        );
        assert!(build_skew_line("0.1.13", "0.1.12", DaemonLifetime::OnDemand).is_some());
    }

    // -----------------------------------------------------------------------
    // The remedy has to match how the daemon was started — BUG-174
    // -----------------------------------------------------------------------

    /// The regression this bug exists for: an always-on daemon runs under
    /// `--shutdown-policy never`, so telling the user to close their sessions
    /// is advice that provably cannot work. It must name `brew services stop`.
    #[test]
    fn an_always_on_daemon_is_told_to_stop_the_service_not_close_sessions() {
        let line = build_skew_line("0.1.16", "0.1.13", DaemonLifetime::AlwaysOnService)
            .expect("a skew must be reported");
        assert!(line.contains("brew services stop teton"), "{line}");
        assert!(
            !line.contains("Exit every teton session"),
            "the on-demand remedy is a no-op against a `--shutdown-policy never` daemon and must \
             not be offered: {line}"
        );
        assert!(line.contains("0.1.16") && line.contains("0.1.13"), "{line}");
        assert_eq!(line.lines().count(), 1, "exactly one line: {line}");
    }

    /// The two lifetimes must not converge on one sentence — if they ever did,
    /// this bug would be back with no test failing.
    #[test]
    fn the_two_lifetimes_give_different_remedies() {
        let on_demand = build_skew_line("0.1.16", "0.1.13", DaemonLifetime::OnDemand).unwrap();
        let service = build_skew_line("0.1.16", "0.1.13", DaemonLifetime::AlwaysOnService).unwrap();
        assert_ne!(on_demand, service);
        // Neither may offer the other's remedy.
        assert!(!on_demand.contains("brew services stop"), "{on_demand}");
        assert!(!service.contains("Exit every teton session"), "{service}");
    }

    /// Agreement is still silence on both lifetimes: the lifetime decides the
    /// remedy, never whether there is anything to report.
    #[test]
    fn matching_builds_say_nothing_on_either_lifetime() {
        for lifetime in [DaemonLifetime::OnDemand, DaemonLifetime::AlwaysOnService] {
            assert_eq!(build_skew_line("0.1.16", "0.1.16", lifetime), None);
        }
    }

    /// A daemon whose version we never learned (no completed handshake) must not
    /// invent one — silence beats a fabricated comparison.
    #[test]
    fn a_connection_without_a_handshake_reports_no_version() {
        assert_eq!(
            build_skew_line("0.1.13", "0.1.13", DaemonLifetime::OnDemand),
            None
        );
        // And the accessor's default is genuinely absent, not an empty string
        // that would compare unequal to every real version.
        assert!(build_skew_line("", "0.1.13", DaemonLifetime::OnDemand).is_some());
    }

    // -----------------------------------------------------------------------
    // The shutting-down refusal is retryable; nothing else is — REQ-565 BR-3
    // -----------------------------------------------------------------------

    #[test]
    fn only_the_shutting_down_code_is_treated_as_retryable() {
        let shutting_down = anyhow::Error::new(RpcError::new(
            error_code::DAEMON_SHUTTING_DOWN,
            "the daemon is shutting down",
        ));
        assert!(is_shutting_down(&shutting_down));

        // A protocol mismatch must NOT be swallowed into a spawn-retry: another
        // daemon from the same binary on disk cannot fix it, so retrying would
        // spin while hiding the one diagnosis that matters.
        let version = anyhow::Error::new(RpcError::new(
            error_code::UNSUPPORTED_PROTOCOL_VERSION,
            "no mutually supported protocol version",
        ));
        assert!(!is_shutting_down(&version));

        for code in [
            error_code::INVALID_PARAMS,
            error_code::INTERNAL_ERROR,
            error_code::UNKNOWN_SESSION,
        ] {
            let err = anyhow::Error::new(RpcError::new(code, "something else"));
            assert!(!is_shutting_down(&err), "code {code} must not be retryable");
        }

        // A transport error carries no RpcError at all.
        assert!(!is_shutting_down(&anyhow!("connection reset")));
    }

    // -----------------------------------------------------------------------
    // REQ-621 ADR-621-1/3/4: the pump wakes, owns the row, closes it out
    // -----------------------------------------------------------------------

    /// An event envelope deserialized from the daemon's own wire form.
    ///
    /// Built by serde from the JSON the bus actually carries rather than as a
    /// struct literal, so a payload the daemon renamed or stopped sending fails
    /// here instead of passing against a value no daemon ever emits
    /// (LESSON-544). The `event` tag is the flattened discriminant the envelope
    /// documents, which is why these are flat objects.
    fn wire_event(json: Value) -> Incoming {
        Incoming::Event(Box::new(
            serde_json::from_value(json).expect("the protocol's own wire form"),
        ))
    }

    /// The wire JSON the daemon's bus emits for `event` in session `session`.
    ///
    /// `EventEnvelope::new(seq, session_id, event)` is the call `tetond`'s
    /// broadcast makes, verbatim, and `to_value` is the serialization the
    /// socket writer performs on the result — so a fixture built here is the
    /// producer's own bytes rather than a second author's guess at them
    /// (LESSON-544). The typed payload is the point: a field that changes shape
    /// changes here too, and a literal that quietly stopped populating one
    /// cannot be written.
    fn published(seq: u64, session: &str, event: teton_protocol::events::Event) -> Value {
        serde_json::to_value(teton_protocol::events::EventEnvelope::new(
            seq,
            Some(teton_protocol::SessionId::from(session)),
            event,
        ))
        .expect("the protocol's own envelope serializes")
    }

    /// The `session/prompt` refusal a fixture's daemon answers with.
    fn turn_refused(id: i64) -> Incoming {
        Incoming::Response(Response::failure(
            Id::Number(id),
            RpcError::new(error_code::INTERNAL_ERROR, "the provider hung up"),
        ))
    }

    /// A `UiContext` over `surface` with a turn already open on `state`.
    ///
    /// `begin_turn` is the one transition no daemon event announces — the
    /// prompt going on the wire — and it is what arms the row's clock
    /// (ADR-621-2). A fixture that skipped it would be testing an idle
    /// projection, which draws nothing whatever the pump does.
    macro_rules! turn_ctx {
        ($surface:ident, $state:ident, $prompter:ident) => {{
            $state.session_id = Some(teton_protocol::SessionId::from("s1"));
            $state.begin_turn("why is the build slow?");
            UiContext {
                surface: &mut $surface,
                state: &mut $state,
                prompter: &mut $prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id: Some(teton_protocol::SessionId::from("s1")),
                skills: crate::slash::SkillSnapshot::empty(),
            }
        }};
    }

    /// **BR-1, both halves.** The row is there through the silent lead-in and
    /// **gone** for as long as reply text is arriving — the arriving text is the
    /// liveness signal, and a spinner beneath a streaming answer is a second
    /// one saying the same thing (BR-10).
    ///
    /// Two ticks before each reply, so the second half is a claim and not an
    /// absence of opportunity: the pump ticks twice *after* the chunk and paints
    /// nothing either time.
    ///
    /// The oracle is a literal sequence of `Rendered` values, spinner glyphs
    /// and all. Composing the expected rows from `TurnActivity::frame` would
    /// pass against a pump that drew the wrong verb at the right moment, and
    /// against a frozen animation (LESSON-569).
    #[test]
    fn the_row_is_present_in_a_silent_phase_and_withdrawn_while_streaming() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(2);
        tx.send(agent_chunk("the finding is")).expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        let answered = conn
            .pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives");

        assert!(answered.is_ok(), "the daemon answered this turn");
        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
                Rendered::Fragment("the finding is".to_owned()),
            ]
        );
        assert!(
            !row.activity_visible && !row.pending_visible,
            "the block was withdrawn for the stream"
        );
        assert_eq!(
            conn.ticks_observed(),
            4,
            "two ticks before each of the two replies"
        );
    }

    /// **BR-4.** The row keeps moving while the daemon says nothing at all.
    ///
    /// This is the acceptance criterion's own sequence: a reply held for three
    /// frames records the row being drawn, repainted twice **in place**, and
    /// then taken back. Before `recv_timeout` the pump sat in
    /// `mpsc::Receiver::recv()` for the whole of a turn, so nothing ran on the
    /// client during a silent stretch and the row could not have moved at all —
    /// the mutation that matters here is reverting the wake, and it reddens on
    /// the two repaints.
    ///
    /// Repaints rather than lines is the load-bearing half: three `Line`s would
    /// satisfy "it moved" while leaving three rows in scrollback, which is what
    /// BR-5 forbids.
    #[test]
    fn the_pump_ticks_while_the_daemon_is_silent() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(3);
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠹ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
            ]
        );
        assert_eq!(conn.ticks_observed(), 3);
    }

    /// **BR-5 / BR-10.** A durable line prints exactly where the row was, and
    /// the row comes back beneath it.
    ///
    /// The tool's own `[running]` line is the thing that must not be pushed
    /// around: it belongs in the log, the row does not, and the row is the one
    /// that moves. `withdraw_row_above` leaves the cursor on the row it
    /// cleared, so the tool line lands there rather than one line below a blank
    /// gap — which is why the row is *withdrawn* rather than repainted empty.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the
    /// withdraw-before-dispatch block in `pump_until_answered`. This fails on
    /// the sequence — left
    ///
    /// ```text
    /// [Line(Activity, "⠋ preparing turn · 0s · turn 0s"),
    ///  Line(Tool, "shell: cargo test [running]"),
    ///  Repaint(1, Activity, "⠙ running shell: cargo test · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠙ running shell: cargo test · 0s · turn 0s")]
    /// ```
    ///
    /// against the sequence below. Every failure the rule exists for is in that
    /// one line: the row is never taken back, so the `[running]` line scrolls in
    /// *below* it — one activity row left in scrollback per durable line (BR-5)
    /// — and the repaint that follows rewrites the **tool's** row instead of the
    /// row's own, so the line the user needed is overwritten by a spinner
    /// (BR-10). It also reddened `the_row_is_present_in_a_silent_phase_…` (the
    /// withdraw lands after the fragment) and `the_pump_ticks_while_…` (no
    /// withdraw at all): 3 red. `every_ends_turn_exit_withdraws_the_row` stayed
    /// **green**, correctly — its row is taken back by `call`'s close-out, which
    /// is the second site and the one BR-12 rests on. Reverted with the same
    /// targeted edit.
    #[test]
    fn a_durable_line_prints_where_the_row_was() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(1);
        tx.send(wire_event(serde_json::json!({
            "session_id": "s1",
            "seq": 3,
            "event": "session_update",
            "update": {
                "kind": "tool_call",
                "tool_call_id": "c1",
                "title": "shell: cargo test",
                "status": "in_progress",
            },
        })))
        .expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
                Rendered::Line(
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
            ]
        );
        assert!(!row.activity_visible && !row.pending_visible);
    }

    /// **BR-1's permission clause.** The question owns the terminal: the row is
    /// withdrawn before it prints, is **not on screen** while the user reads
    /// it, and comes back naming the phase the question interrupted.
    ///
    /// The restore is the part that needs a test rather than an argument. The
    /// phase after a permission answer is not derivable from what arrives next
    /// — the next event may be minutes away, and a tool that was running is
    /// still running — so `TurnActivity::permission_answered` remembers it, and
    /// the pump is the one caller that knows the question is over.
    #[test]
    fn a_permission_question_owns_the_terminal_and_the_row_returns_to_its_phase() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(1);
        tx.send(wire_event(serde_json::json!({
            "session_id": "s1",
            "seq": 4,
            "event": "session_update",
            "update": {
                "kind": "tool_call",
                "tool_call_id": "c1",
                "title": "shell: cargo test",
                "status": "in_progress",
            },
        })))
        .expect("queue");
        tx.send(permission_envelope("shell")).expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            ctx.state.activity.phase(),
            crate::activity::Phase::ToolRunning,
            "the phase the question interrupted is the phase it came back to"
        );
        assert!(!row.activity_visible && !row.pending_visible);
        assert_eq!(
            surface.calls,
            vec![
                // The silent lead-in.
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                // The tool starts: row down, the durable line where it was, row
                // back beneath it naming the tool.
                Rendered::Withdraw(1),
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
                Rendered::Line(
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                // The question. Nothing of the row is between the withdraw and
                // the question's own line: the prompt reaches a `Prompter`,
                // which writes straight to stdout and could not have taken a
                // row back for itself.
                Rendered::Withdraw(1),
                Rendered::Line(
                    LineKind::Prompt,
                    "permission requested: shell — run `cargo test`".to_owned()
                ),
                // Answered — and the row is back on the **tool**, which is what
                // the turn is still doing. A phase re-derived from the next
                // event could not have said so: the next event may be minutes
                // away, and nothing on the wire announces "the question is
                // over".
                Rendered::Line(
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                // And the response takes it back for good.
                Rendered::Withdraw(1),
            ]
        );
    }

    /// **BR-9's second clause, and BR-5's one recorded exception.** A line the
    /// user has **submitted** mid-turn takes the row out for the rest of the
    /// turn rather than painting over what they typed.
    ///
    /// The geometry is the whole of it. `ECHO` stays on for the length of a
    /// turn, so the terminal — not this process — echoes the user's characters
    /// into the row below the activity row, and echoes a newline when they
    /// press Enter. The cursor drops a line that no bookkeeping here can see,
    /// and from that moment `repaint_row_above(1)` rewrites *the line holding
    /// what they typed* and `withdraw_row_above(1)` erases it outright. Neither
    /// is acceptable, and no offset correction is available: the row is already
    /// unreachable, since a client in canonical mode cannot know how many rows
    /// the echo wrapped onto.
    ///
    /// So the row is abandoned, the last frame stays in scrollback — the
    /// bounded BR-5 exception this pass recorded in the spec — and every
    /// durable line the rest of the turn prints still prints.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the abandon
    /// block from `pump_until_answered`. **1 red of 828**, this test, on the
    /// sequence, which becomes
    ///
    /// ```text
    /// [Line(Activity, "⠋ preparing turn · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠋ preparing turn · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠙ preparing turn · 0s · turn 0s"),
    ///  Withdraw(1),
    ///  Line(Tool, "shell: cargo test [running]"),
    ///  Line(Activity, "⠹ running shell: cargo test · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠹ running shell: cargo test · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠸ running shell: cargo test · 0s · turn 0s"),
    ///  Withdraw(1)]
    /// ```
    ///
    /// — four repaints and two withdraws, every one of them aimed a row above
    /// where the pump believes the cursor is, which at a real terminal is the
    /// user's own line six times over. That it is the *only* red is the
    /// measure of the gap: every other row test runs with `line_waiting`
    /// answering `false`, which is what a user who is not typing looks like,
    /// and no leg at a real terminal types *and* submits while a row is up.
    /// Reverted with the same targeted edit.
    #[test]
    fn a_submitted_line_abandons_the_row_and_never_paints_over_it() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);

        // A row on screen, drawn the way the pump draws one.
        row.paint_rows(&mut ctx, Instant::now());
        assert!(row.activity_visible, "the fixture has a row to lose");

        // The user types and presses Enter. From here on stdin has a line
        // waiting and the cursor is one row lower than the pump thinks.
        row.line_waiting = || true;

        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(2);
        tx.send(wire_event(serde_json::json!({
            "session_id": "s1",
            "seq": 3,
            "event": "session_update",
            "update": {
                "kind": "tool_call",
                "tool_call_id": "c1",
                "title": "shell: cargo test",
                "status": "in_progress",
            },
        })))
        .expect("queue");
        tx.send(turn_answered(1)).expect("queue");
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                // The frame that was already on screen when Enter was pressed.
                // It stays there: the exception BR-5 now records.
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                // And the turn goes on writing what belongs in the log.
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
            ],
            "no repaint and no withdraw may follow a submitted line"
        );
        assert!(
            !row.live && !row.activity_visible && !row.pending_visible,
            "the row is given up for the rest of the turn, not for one frame"
        );
    }

    /// **BR-13.** A terminal that will not take the row's bytes costs the turn
    /// its row and nothing else — and says so in verbose mode.
    ///
    /// Both halves are the rule. A row abandoned in silence is
    /// indistinguishable from a daemon that has gone quiet, which is the
    /// ambiguity BR-11's annotation exists to remove: the user would read "the
    /// daemon is wedged" off a fact about their terminal. And the turn itself
    /// is untouched — the response still arrives, still returns, and the
    /// summary is still read.
    ///
    /// The surface here takes `line` and refuses the row verbs, which is the
    /// terminal this rule is about; `render.rs` pins that `PlainSurface`
    /// actually reports a failed `write!`/`flush` as `false`
    /// (`a_row_verb_reports_whether_its_bytes_landed`), and the trait's default
    /// answer is pinned beside it. Together those are BR-13 end to end.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** ignore the report
    /// in `paint_row` (`let _ = ctx.surface.repaint_row_above(1,
    /// LineKind::Activity, &text);` in place of the `if !…` block). **1 red of
    /// 828**, this test, on the repaint count — two repaints instead of one,
    /// `row.live` still true, and no verbose line: the row painting on into a
    /// terminal that has stopped taking it, which is BR-13 implemented as
    /// zero. Reverted with the same targeted edit.
    #[test]
    fn a_failed_paint_hides_the_row_and_says_so_in_verbose() {
        for verbose in [false, true] {
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(3);
            tx.send(turn_answered(1)).expect("queue");

            let mut surface = RecordingSurface::with_failing_rows();
            let mut state = SessionState::new();
            state.verbose = verbose;
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut row = RowState::at_width(80);
            let mut ctx = turn_ctx!(surface, state, prompter);
            let answered = conn
                .pump_until_answered(turn_params(), &mut ctx, &mut row)
                .expect("a refused row must not fail the turn");

            assert!(
                answered.is_ok(),
                "the turn itself is untouched (verbose {verbose})"
            );
            assert_eq!(
                surface
                    .calls
                    .iter()
                    .filter(|call| matches!(call, Rendered::Repaint(..)))
                    .count(),
                1,
                "one repaint is attempted, refused, and never attempted again \
                 (verbose {verbose}): {:?}",
                surface.calls
            );
            assert!(
                !row.live,
                "the row is abandoned for the rest of the turn (verbose {verbose})"
            );
            assert_eq!(
                surface.any_line_contains(
                    LineKind::Info,
                    "activity row: terminal write failed; hidden for the rest of the turn"
                ),
                verbose,
                "the failure is recorded in verbose output and nowhere else \
                 (verbose {verbose}): {:?}",
                surface.calls
            );
        }
    }

    /// A row **drawn** after a resize is fitted to the terminal as it is now.
    ///
    /// The width was read once per call until this pass, which is right for the
    /// frames of one row — a resize between two of them costs a truncation a
    /// column early, and re-reading would be a `TIOCGWINSZ` eight times a
    /// second for the whole of every turn — and wrong between two *rows*. Every
    /// durable line a turn prints puts one row away and brings another back, so
    /// a turn that outlives a resize would go on fitting new rows to a window
    /// that no longer exists, and a row wider than the terminal is hard-wrapped
    /// into a second row the withdraw cannot clear (BR-5).
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the
    /// `row.width = (row.measure_width)();` line from `paint_row`. **2 red**:
    /// this test, on the second row still being fitted to the 60-column
    /// terminal that is now 24 columns wide, and
    /// `every_ends_turn_exit_withdraws_the_row`, which drives the real `call`
    /// — `RowState::new` no longer measures at construction, so with the
    /// re-read gone its rows are fitted to zero columns and never drawn at all.
    /// The second red is the honest shape of the change: the measurement moved
    /// from the constructor to the draw, and deleting it there leaves nothing
    /// measuring anywhere. Reverted with the same edit.
    #[test]
    fn a_row_drawn_after_a_resize_is_fitted_to_the_new_width() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(60);
        let mut ctx = turn_ctx!(surface, state, prompter);
        // A route, so the sentence is long enough for a narrow terminal to cut.
        ctx.state.observe_activity(
            &teton_protocol::events::EventEnvelope::new(
                1,
                Some(teton_protocol::SessionId::from("s1")),
                teton_protocol::events::Event::RouteDecided(teton_protocol::events::RouteDecided {
                    category: None,
                    tier: Some(teton_protocol::Tier::Think),
                    phase: None,
                    provider_id: teton_protocol::ProviderId::from("anthropic"),
                    model: Some("claude-opus-5".to_owned()),
                    reason: "fixture".to_owned(),
                    effort: None,
                    window_tokens: None,
                    budget_tokens: None,
                    budget_bytes: None,
                    bound: None,
                    spend_ceiling_micro_cents: None,
                    bound_floored: None,
                    repo_context_cap: None,
                }),
            ),
            Instant::now(),
        );

        row.paint_rows(&mut ctx, Instant::now());
        // The pump withdraws before every durable line, so the next frame of
        // this turn is a fresh draw rather than a repaint.
        row.activity_visible = false;
        set_test_width(24);
        row.paint_rows(&mut ctx, Instant::now());

        let drawn: Vec<&str> = surface
            .calls
            .iter()
            .filter_map(|call| match call {
                Rendered::Line(LineKind::Activity, text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(drawn.len(), 2, "two rows were drawn: {:?}", surface.calls);
        assert!(
            crate::markdown::display_width(drawn[0]) > 23,
            "the first row used the whole of the wide terminal: {:?}",
            drawn[0]
        );
        assert!(
            crate::markdown::display_width(drawn[1]) <= 23,
            "the second row is fitted to the terminal as it is now: {:?}",
            drawn[1]
        );
    }

    /// A reset event stream, arriving while the row is up: the row comes down,
    /// the notice prints where it was, and the row comes back beneath it.
    ///
    /// `Incoming::Lagged` is the third arm of the pump's message match and the
    /// only one that renders without an envelope, so it is the arm most easily
    /// left out of the row's discipline — a notice printed under a live row
    /// would be scrolled away by the next repaint, and the row would be left in
    /// the log (BR-5, BR-10).
    ///
    /// **Mutation, applied and observed red (2026-09-10):** gate the
    /// withdraw-before-dispatch on the message being an event
    /// (`if row.visible && matches!(message, Incoming::Event(_))`), so the lag
    /// arm keeps its row. **4 red**: this test, plus
    /// `a_durable_line_prints_where_the_row_was`,
    /// `a_permission_question_owns_the_terminal_…` and
    /// `the_pump_ticks_while_the_daemon_is_silent`, which lose the withdraw
    /// that takes the row back on the *response*. Those three are why the
    /// shared block is right; this test is why the block has to be shared, and
    /// it is the only one of the four that fails on the lag arm itself.
    #[test]
    fn a_reset_event_stream_prints_where_the_row_was() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(1);
        tx.send(Incoming::Lagged(RpcError::new(
            error_code::INTERNAL_ERROR,
            "the client fell 42 events behind",
        )))
        .expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
                Rendered::Line(
                    LineKind::Error,
                    "event stream lagged and was reset: the client fell 42 events behind"
                        .to_owned()
                ),
                Rendered::Line(
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s".to_owned()
                ),
                // The tick that precedes the response, in place as always.
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::Withdraw(1),
            ],
            "the lag notice belongs in the log; the row is what moves"
        );
        assert!(!row.activity_visible && !row.pending_visible);
    }

    /// **BR-4's and BR-6's benign path.** With no live rows the pump keeps the
    /// blocking receive it has always had, so it **never enters the tick arm**
    /// — not once, not to find nothing to draw.
    ///
    /// The tick counter is what makes that a claim rather than a coincidence. A
    /// recording with no activity row in it would pass just as happily against
    /// a pump that woke eight times a second on a piped session; zero ticks is
    /// the property AC-3 rests on, and it is why the gate is read from the
    /// surface once, into [`RowState::live`], rather than asked at each draw.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** force
    /// `RowState::new`'s gate to `true` (`let live = true;` in place of
    /// `surface.has_live_rows()`). This is the only test in the suite that
    /// fails — 1 red of 816 — and it fails naming the whole defect:
    ///
    /// ```text
    /// a piped session emits no row, no repaint and no withdraw:
    /// [Line(Activity, "⠋ preparing turn · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠙ preparing turn · 0s · turn 0s"),
    ///  Repaint(1, Activity, "⠹ preparing turn · 0s · turn 0s"),
    ///  Withdraw(1)]
    /// ```
    ///
    /// A whole animation, cursor escapes and all, on a surface that never
    /// claimed it could take a row back. That it is the *only* red is the
    /// measure of how little else guards BR-6: `PlainSurface` would have
    /// swallowed the bytes on its own gate, so every pipe fixture in the e2e
    /// suites stays green while the pump is doing this. Reverted with the same
    /// targeted edit.
    #[test]
    fn a_plain_surface_never_enters_the_tick_arm() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(3);
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::new();
        let mut row = RowState::new(&surface);
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert!(
            surface.calls.is_empty(),
            "a piped session emits no row, no repaint and no withdraw: {:?}",
            surface.calls
        );
        assert_eq!(
            conn.ticks_observed(),
            0,
            "the timed receive was never called, so the tick arm is unreachable"
        );

        // The gate itself, after the behaviour it decides: the answer comes from
        // the **surface**, so a recorder that claims live rows arms one and a
        // recorder that does not, does not.
        assert!(
            !row.live,
            "a recorder that claims no live rows must not arm one"
        );
        assert!(
            RowState::new(&RecordingSurface::with_live_rows()).live,
            "...and one that claims them must: the gate is the surface's answer, \
             not a constant"
        );
    }

    /// **BR-9.** The tick arm adds nothing to the latency of a message that is
    /// already queued.
    ///
    /// `recv_timeout` consults the channel before it consults the clock, so a
    /// waiting event is returned on the same call that would otherwise have
    /// timed out — there is deliberately no `try_recv` ahead of it, which would
    /// be a second read of the same fact. With both replies pre-filled and no
    /// delay armed, **every** wake is a message and no frame interval is spent:
    /// zero ticks, and no row was ever drawn because the pump never had a quiet
    /// moment to draw one in.
    #[test]
    fn the_tick_arm_adds_no_latency_to_a_queued_message() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(agent_chunk("straight through")).expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80);
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            conn.ticks_observed(),
            0,
            "a queued message must not wait out a frame interval"
        );
        assert_eq!(
            surface.calls,
            vec![Rendered::Fragment("straight through".to_owned())]
        );
    }

    /// **BR-12.** Every way an `ENDS_TURN` call can return leaves no row and a
    /// summary to read — a normal result, an `RpcError`, and a daemon that went
    /// away mid-turn.
    ///
    /// The three legs exit through **two different sites**, which is the point.
    /// A response — success or refusal — is a message, so the pump's
    /// withdraw-before-dispatch takes the row back on its way out. A dropped
    /// socket is a `?` out of the middle of the pump, and only `call`'s
    /// close-out runs on it: the row's state is owned there and lent to the
    /// pump for exactly that reason (ADR-621-4). Termination therefore depends
    /// on nothing the daemon sends.
    ///
    /// The row's text is left as `_` here and pinned literally in the three
    /// tests above: this leg is about which verbs ran, and it drives the real
    /// `call`, whose width comes from the terminal the developer happens to be
    /// in.
    #[test]
    fn every_ends_turn_exit_withdraws_the_row() {
        for (leg, reply) in [
            ("a normal result", Some(turn_answered(1))),
            ("an RpcError", Some(turn_refused(1))),
            ("a dropped socket", None),
        ] {
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(1);
            match reply {
                Some(reply) => tx.send(reply).expect("queue"),
                // The reader thread is gone: `recv_timeout` reports
                // `Disconnected`, which is the same sentence `recv` produces.
                None => drop(tx),
            }

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = turn_ctx!(surface, state, prompter);
            let outcome = conn.call(turn_params(), &mut ctx);

            assert!(
                matches!(
                    surface.calls.as_slice(),
                    [Rendered::Line(LineKind::Activity, _), Rendered::Withdraw(1)]
                ),
                "{leg} left a row behind: {:?}",
                surface.calls
            );
            assert!(
                state.last_turn_summary.is_some(),
                "{leg} must still report what the turn spent (BR-16)"
            );
            assert_eq!(
                state.activity.phase(),
                crate::activity::Phase::Idle,
                "{leg} must leave no turn in flight"
            );
            match leg {
                "a dropped socket" => assert_eq!(
                    outcome.expect_err("the connection is gone").to_string(),
                    "connection to the daemon closed",
                    "the timed receive reports a closed channel in the words \
                     the blocking one does"
                ),
                "a normal result" => assert!(outcome.expect("the response arrives").is_ok()),
                _ => assert!(outcome.expect("the response arrives").is_err()),
            }
        }
    }

    /// **BR-12's benign path.** A non-turn RPC pumping through the same loop on
    /// the same terminal draws no row at all.
    ///
    /// Whether there is a row is the **projection's** decision, never the
    /// method's (ADR-621-1): a `/cost`, a `/model`, a `config/get` finds the
    /// activity idle, and an idle projection has no frame. The pump still ticks
    /// — the surface has live rows and the clock still runs — which is what
    /// makes the empty recording a statement about the projection rather than
    /// about the gate, and it is why there is no `P::ENDS_TURN` test anywhere
    /// near the draw.
    ///
    /// It also pins the other half of the close-out: no summary, because no
    /// turn ended.
    #[test]
    fn a_non_turn_method_never_draws_or_withdraws() {
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(2);
        // A real event, dispatched mid-call, so the message path is exercised
        // too: it folds into an idle projection and changes nothing.
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Ready,
        ))
        .expect("queue");
        tx.send(Incoming::Response(Response::failure(
            Id::Number(1),
            RpcError::new(error_code::METHOD_NOT_FOUND, "no such method"),
        )))
        .expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        state.session_id = Some(teton_protocol::SessionId::from("s1"));
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: Some(teton_protocol::SessionId::from("s1")),
            skills: crate::slash::SkillSnapshot::empty(),
        };
        let refused = conn
            .call(methods::ConfigGetParams::default(), &mut ctx)
            .expect("the response arrives");

        assert!(refused.is_err(), "this fixture's daemon refuses the read");
        assert!(
            !surface.calls.iter().any(|call| matches!(
                call,
                Rendered::Line(LineKind::Activity, _)
                    | Rendered::Repaint(_, LineKind::Activity, _)
                    | Rendered::Withdraw(_)
            )),
            "a non-turn RPC drew or withdrew a row: {:?}",
            surface.calls
        );
        assert!(
            state.last_turn_summary.is_none(),
            "no turn ended, so there is nothing to report about one"
        );
        assert_eq!(
            conn.ticks_observed(),
            4,
            "the pump did tick — twice before each reply — and found nothing to \
             draw, which is the projection's decision and not the method's"
        );
    }

    // -----------------------------------------------------------------------
    // REQ-622 ADR-622-1/4: the pump reads the keyboard and owns two rows
    // -----------------------------------------------------------------------

    /// **BR-1 / AC-7: the terminal's mode changes for a turn at a terminal, and
    /// for nothing else.**
    ///
    /// Four legs, and three of them are the ones that matter. "No `tcsetattr`
    /// was called" is a claim about a call that did **not** happen, and the only
    /// honest way to assert one is to count the calls that did — an empty
    /// recording would pass equally against a pump that took the terminal and
    /// then found nothing to draw, which is the failure AC-7 is about (a piped
    /// session whose behaviour must be byte-identical to today's).
    ///
    /// The positive leg asserts the *consequence* rather than the count alone:
    /// bytes typed during the turn reach a row of their own. Its row text is
    /// the only literal here, and a short one, because this leg drives the real
    /// [`Connection::call`] — whose width comes from whichever terminal the
    /// developer is sitting in (see [`RowState::at_width`]).
    #[test]
    fn raw_mode_is_engaged_only_for_a_turn_at_a_terminal() {
        // A turn, a live-row surface, a terminal on stdin: asked once, taken,
        // and read from.
        {
            script_raw_mode(InputVerdict::Raw);
            script_keystrokes(&[b"hi"]);
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(1);
            tx.send(turn_answered(1)).expect("queue");

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = turn_ctx!(surface, state, prompter);
            conn.call(turn_params(), &mut ctx)
                .expect("the response arrives")
                .expect("the daemon answered");

            assert_eq!(
                engage_attempts(),
                1,
                "a turn at a terminal asks for the input exactly once, at the \
                 start of the call"
            );
            assert!(
                surface.calls.iter().any(|call| matches!(
                    call,
                    Rendered::DrawCurrent(LineKind::Pending, text) if text == "> hi"
                )),
                "the pump read the keystrokes and drew them on a row of its \
                 own — its own class, and the verb that leaves the cursor at \
                 the end of what was typed: {:?}",
                surface.calls
            );
        }

        // A non-turn RPC on the same terminal. The row is the projection's
        // decision, but the *mode* is the method's, and thirty of `call`'s
        // callers are not turns.
        {
            script_raw_mode(InputVerdict::Raw);
            script_keystrokes(&[b"hi"]);
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(1);
            tx.send(Incoming::Response(Response::failure(
                Id::Number(1),
                RpcError::new(error_code::METHOD_NOT_FOUND, "no such method"),
            )))
            .expect("queue");

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = turn_ctx!(surface, state, prompter);
            let refused = conn
                .call(methods::ConfigGetParams::default(), &mut ctx)
                .expect("the response arrives");

            assert!(refused.is_err(), "this fixture's daemon refuses the read");
            assert_eq!(
                engage_attempts(),
                0,
                "a `/config`, a `/cost` or a `/model` must not touch the \
                 terminal's mode: {:?}",
                surface.calls
            );
        }

        // A turn whose stdout is a pipe. The surface's own gate answers first,
        // so this never even asks.
        {
            script_raw_mode(InputVerdict::Raw);
            script_keystrokes(&[b"hi"]);
            let (mut conn, tx, _peer) = test_connection();
            tx.send(turn_answered(1)).expect("queue");

            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = turn_ctx!(surface, state, prompter);
            conn.call(turn_params(), &mut ctx)
                .expect("the response arrives")
                .expect("the daemon answered");

            assert_eq!(engage_attempts(), 0, "a piped surface asks for nothing");
            assert!(
                surface.calls.is_empty(),
                "and emits nothing: {:?}",
                surface.calls
            );
        }

        // AC-7 exactly: a **piped stdin** with a terminal stdout. The row is
        // live, the pump ticks, and the mode is left alone — the two halves of
        // "at a terminal" are two different descriptors and both are required.
        {
            script_raw_mode(InputVerdict::Raw);
            script_keystrokes(&[b"hi"]);
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(1);
            tx.send(turn_answered(1)).expect("queue");

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            state.session_id = Some(teton_protocol::SessionId::from("s1"));
            state.begin_turn("why is the build slow?");
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: false,
                session_id: Some(teton_protocol::SessionId::from("s1")),
                skills: crate::slash::SkillSnapshot::empty(),
            };
            conn.call(turn_params(), &mut ctx)
                .expect("the response arrives")
                .expect("the daemon answered");

            assert_eq!(
                engage_attempts(),
                0,
                "a piped stdin with a terminal stdout never calls `tcsetattr` \
                 (AC-7)"
            );
            assert!(
                !surface.calls.iter().any(|call| matches!(
                    call,
                    Rendered::Line(LineKind::Activity, text) if text.starts_with('>')
                )),
                "and draws no pending row, because nothing is typed at a pipe: \
                 {:?}",
                surface.calls
            );
            assert!(
                surface
                    .calls
                    .iter()
                    .any(|call| matches!(call, Rendered::Line(LineKind::Activity, _))),
                "the activity row is unaffected — this leg is about stdin: {:?}",
                surface.calls
            );
        }
    }

    /// **BR-2: one reader of stdin, still — and this module names exactly one
    /// way to read it.**
    ///
    /// Structural, because the property is about code that does *not* exist. A
    /// behavioural test can show that the bytes the pump got came from the
    /// hook; it cannot show that no second path to the descriptor was added
    /// beside it, and a second reader is precisely how REQ-556's
    /// one-reader-of-stdin rule would come undone — two readers of one terminal
    /// split a keystroke between them, and neither can be told it happened.
    ///
    /// The needles are the three shapes a second reader takes in this
    /// codebase's vocabulary: `std::io::stdin()`, a raw `libc` call, and the
    /// descriptor number itself. All three are legitimate **in `prompt.rs`**,
    /// which is the seam; none of them belongs here.
    #[test]
    fn the_pump_reads_only_through_read_available() {
        let client = crate::status::scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "client.rs")
            .map(|(_, src)| crate::status::scan::code_only(&src))
            .expect("client.rs is a production source");

        // Non-vacuity, both halves: the sweep is reading this module's real
        // production text, and the one reader it is allowed to name is there.
        assert!(
            client.contains("fn pump_until_answered"),
            "the sweep is not reading client.rs any more"
        );
        assert_eq!(
            client.matches("crate::prompt::read_available").count(),
            1,
            "the pump reads the keyboard through **one** named seam, installed \
             once as `RowState::read_available` — a second call site is a second \
             answer to \"what has the user typed\" and the two would disagree \
             about a keystroke one of them consumed"
        );

        for needle in ["io::stdin", "libc::", "STDIN_FILENO"] {
            assert!(
                !client.contains(needle),
                "client.rs names `{needle}`. Reading the terminal is \
                 `prompt.rs`'s job and the pump's one seam into it is \
                 `read_available` (BR-2); a descriptor reached from here would \
                 be a second reader of a terminal that cannot report having \
                 been read twice, and would be invisible to the `Prompter` \
                 front-end those seams exist for"
            );
        }

        // And the seam itself, behaviourally, on the one input it can answer
        // with no descriptor involved at all: handed no room, it reads nothing.
        // That keeps the assertions above from being a claim about a function
        // this build does not have.
        assert_eq!(
            crate::prompt::read_available(&mut []).expect("no room is not an error"),
            0
        );
    }

    /// **BR-3 / BR-13: the pending row is the client's, and nothing the pump
    /// draws or takes back ever lands on it.**
    ///
    /// The oracle is a literal sequence of `Rendered` values, offsets and
    /// spinner glyphs and all, and every offset in it is the load-bearing part:
    /// `Repaint(2, ..)` for the activity row while a pending row is beneath it,
    /// `Repaint(1, ..)` for the pending row itself, and both rows withdrawn —
    /// bottom first — before the tool's durable line prints. Composing the
    /// expected rows from `TurnActivity::frame` and `InputEditor::row` would
    /// pass against a pump that drew the right text at the wrong offset, which
    /// is the entire failure (LESSON-569).
    ///
    /// This is what BUG-225 could not be fixed without. In canonical mode the
    /// user's characters were echoed by the *terminal* into the row below the
    /// row, so the activity row's own offset was wrong the instant anything was
    /// typed and the only safe move was to stop painting (REQ-621's recorded
    /// exception). Here the pending row is the pump's own, at an offset the
    /// pump chose, so the geometry cannot go stale: every keystroke arrives as
    /// bytes this loop read.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** fix the activity
    /// row's repaint offset at 1 (`let rows_up = 1;` in place of the arithmetic
    /// the block does over the two visibilities). **2 red of 853** — this test
    /// and `a_question_reads_a_fresh_buffer_…`, on `Repaint(1, ..)` where the
    /// oracle says `Repaint(2, ..)` at every activity repaint that has a
    /// pending row beneath it. At a real terminal each of those rewrites the
    /// line the user is typing into, eight times a second: BUG-225's failure
    /// exactly, arriving through the fix for it. Nothing else in the suite
    /// noticed, because every other row test runs with no pending row and the
    /// offset is 1 either way — which is why the oracle is a literal sequence
    /// including its offsets, rather than the row texts alone. Reverted with
    /// the same targeted edit.
    #[test]
    fn the_pending_row_is_never_painted_over() {
        script_keystrokes(&[b"why", b" not"]);
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(2);
        tx.send(wire_event(serde_json::json!({
            "session_id": "s1",
            "seq": 3,
            "event": "session_update",
            "update": {
                "kind": "tool_call",
                "tool_call_id": "c1",
                "title": "shell: cargo test",
                "status": "in_progress",
            },
        })))
        .expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                // The block scrolls in, top row first.
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why".to_owned()),
                // A second keystroke: both rows repainted in place, the
                // activity row **one** up — the pending row is the row the
                // cursor is on, so it adds no offset — and the pending row in
                // place, with no offset of its own at all.
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why not".to_owned()),
                // A durable line: the whole block comes down, bottom first —
                // the current row cleared where the cursor is, then the row
                // above it — and the tool's line prints where the block was.
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
                // ...and the block comes back beneath it, the user's half-typed
                // line intact.
                Rendered::Line(
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why not".to_owned()),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why not".to_owned()),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠸ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why not".to_owned()),
                // And the response takes the block back for good.
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
            ]
        );
        assert!(
            !row.activity_visible && !row.pending_visible,
            "the block is off the screen when the pump returns"
        );
        assert!(
            row.live,
            "and nothing was abandoned: a client that owns the input has no \
             cursor move to miss (BR-4)"
        );
    }

    /// **REQ-622 ADR-622-4, verify: the pending row is the row the cursor is
    /// on, and the activity row is exactly one above it.**
    ///
    /// ADR-622-4 has said "the cursor rests at the end of the pending row" since
    /// architecture; the block drew it with a trailing newline and parked the
    /// cursor on the blank row *below* the whole thing. That is not a cosmetic
    /// difference at a terminal — the caret is the only thing on screen that
    /// says where typing goes, and it was pointing at a row nobody owns — and it
    /// put every offset above it one row further away than it needed to be.
    ///
    /// Driven straight through `paint_rows` rather than through the pump, so
    /// the oracle is the geometry alone: same `now`, same tick, so the activity
    /// row's text cannot move and the only thing that changes between the two
    /// passes is what the user typed. The literal sequence is the whole claim
    /// ([[LESSON-569]]) — the verbs, in order, with their offsets:
    ///
    /// * the activity row scrolls in as a **line**, so the cursor steps past it;
    /// * the pending row is drawn last, as the **current** row;
    /// * a keystroke repaints the activity row **one** up — not two — and the
    ///   pending row in place with no offset at all;
    /// * and the block comes down bottom-first, the current row cleared where
    ///   the cursor already is.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** restore the old
    /// offset arithmetic at the activity repaint (`let rows_up = if
    /// self.pending_visible { 2 } else { 1 };`). **3 red of 872** — this test,
    /// `the_pending_row_is_never_painted_over` and
    /// `a_question_reads_a_fresh_buffer_and_the_pending_line_comes_back`, all
    /// on `Repaint(2, ..)` where the oracle says `Repaint(1, ..)`; the three are
    /// the whole of the suite that paints an activity row with a pending row
    /// beneath it. At a real terminal that repaint
    /// lands two rows up from a cursor that is only one row down: it rewrites
    /// whatever sits *above* the activity row — the last durable line of the
    /// turn — eight times a second. Reverted with the same targeted edit.
    #[test]
    fn the_pending_row_is_the_row_the_cursor_is_on() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        {
            let mut ctx = turn_ctx!(surface, state, prompter);
            let now = Instant::now();
            ctx.state.input.push(b"why");
            row.paint_rows(&mut ctx, now);
            ctx.state.input.push(b" not");
            row.paint_rows(&mut ctx, now);
            assert!(row.withdraw_rows(&mut ctx), "both rows came down");
        }

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why".to_owned()),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why not".to_owned()),
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
            ]
        );
    }

    /// **REQ-622, verify: a pending row redrawn because the activity row
    /// *left* is fitted to the terminal as it is now.**
    ///
    /// [`RowState::width`] states the rule — a row about to be **drawn** is
    /// measured, a row being repainted is not — and gives the reason: between
    /// two rows a stale width fits the new row to a window that no longer
    /// exists, and a row wider than the terminal hard-wraps into a second row
    /// `withdraw_row_above(1)` cannot clear. The block measured when a row
    /// *appeared* and nowhere else, so the one door that was not watched was the
    /// other half of the same reflow: a row cannot be inserted above one already
    /// on screen, so an activity row **leaving** also takes the pending row down
    /// and draws it again — and that draw got the width from before the resize.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** drop the
    /// `|| (self.pending_visible && activity_leaves)` clause from `paint_rows`'
    /// width condition. **1 red of 872**, this test: the redrawn row comes back
    /// as the full `> 0123…xyz` fitted to 80 on a terminal now 24 wide.
    /// Reverted with the same edit.
    #[test]
    fn a_pending_row_redrawn_when_the_activity_row_leaves_is_refitted() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        {
            let mut ctx = turn_ctx!(surface, state, prompter);
            let now = Instant::now();
            ctx.state
                .input
                .push(b"0123456789abcdefghijklmnopqrstuvwxyz");
            row.paint_rows(&mut ctx, now);

            // The window narrows, and the reply starts arriving — so the
            // activity row leaves, which is what takes the pending row down and
            // puts it back.
            set_test_width(24);
            the_reply_starts_streaming(&mut ctx, now);
            row.paint_rows(&mut ctx, now);
        }

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(
                    LineKind::Pending,
                    "> 0123456789abcdefghijklmnopqrstuvwxyz".to_owned()
                ),
                // The reflow: bottom row first, then the row above it, then the
                // pending row again — **fitted to 24**, so the marker, the
                // twenty-one columns that fit, and the column the cursor rests
                // in.
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
                Rendered::DrawCurrent(LineKind::Pending, "> fghijklmnopqrstuvwxyz".to_owned()),
            ]
        );
    }

    /// **REQ-622, verify: BR-14's count survives the one phase that draws no
    /// activity row.**
    ///
    /// `TurnActivity::frame` answers `None` for the whole of `Streaming` — the
    /// arriving reply is its own liveness signal, and a spinner beneath it would
    /// be a second one saying the same thing (ADR-621-1). So the row BR-14 hangs
    /// its `· N queued` clause on is not on screen for most of a long answer,
    /// and an Enter pressed there took the pending row down and said nothing at
    /// all — which is precisely what a swallowed keystroke looks like, and the
    /// ambiguity BR-14 exists to remove.
    ///
    /// The count moves to the pending row, and **only** there: the hint is
    /// `None` whenever the activity row is due, so the two can never both report
    /// it. That second half is the leg after the stream, where the tool row
    /// comes back and the marker goes plain again.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** pass `None` for
    /// `queued_hint` unconditionally in `paint_rows`. **1 red of 872**, this
    /// test: nothing is drawn at all after the Enter — no pending row, no
    /// activity row — which is the defect exactly. Reverted with the same edit.
    #[test]
    fn a_line_queued_while_the_reply_streams_is_still_reported() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        {
            let mut ctx = turn_ctx!(surface, state, prompter);
            let now = Instant::now();
            the_reply_starts_streaming(&mut ctx, now);

            // Typed and submitted while the answer is arriving.
            ctx.state.input.push(b"and the tests?");
            row.paint_rows(&mut ctx, now);
            ctx.state.input.push(b"\n");
            ctx.state.activity.set_queued(ctx.state.input.queued_len());
            row.paint_rows(&mut ctx, now);
        }

        assert_eq!(
            surface.calls,
            vec![
                // No activity row above it: in `Streaming` there is none.
                Rendered::DrawCurrent(LineKind::Pending, "> and the tests?".to_owned()),
                // Enter. The line is gone from the buffer, and the row that is
                // left says where it went.
                Rendered::RepaintCurrent(LineKind::Pending, "[1 queued] > ".to_owned()),
            ]
        );
        assert_eq!(state.input.queued_len(), 1);
    }

    /// The other half of the rule above: with an activity row due, the count is
    /// the **clause's** and the marker is plain.
    ///
    /// Asserted separately because it is the half a hint threaded
    /// unconditionally would break, and it would break invisibly — two places
    /// saying "1 queued" reads as two lines waiting.
    #[test]
    fn the_queued_count_is_never_reported_twice() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        {
            let mut ctx = turn_ctx!(surface, state, prompter);
            let now = Instant::now();
            ctx.state.input.push(b"ship it\n");
            ctx.state.input.push(b"and then");
            ctx.state.activity.set_queued(ctx.state.input.queued_len());
            row.paint_rows(&mut ctx, now);
        }

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s · 1 queued".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> and then".to_owned()),
            ],
            "the clause carries the count and the marker stays plain"
        );
    }

    /// **REQ-622 BR-5, verify: the kernel's own input queue is discarded at the
    /// shelve, before the question is drawn.**
    ///
    /// The shelve accounts for every byte the **pump** has read, and the pump
    /// has just read everything the descriptor was holding — `read_keys` loops
    /// until it reports nothing left. What the shelve cannot account for is what
    /// arrived in the window between that read and this question's first row:
    /// those bytes are in the kernel, not in the editor, and the question's
    /// first `read(2)` would take them as its answer. BR-5 says a question reads
    /// only what was typed after it was drawn; the shelve says that of the
    /// editor and the flush says it of the kernel.
    ///
    /// Observed **inside** the closure, which is where the ordering claim lives:
    /// a flush after `ask` returns would be a flush of the answer.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** delete the
    /// `crate::prompt::discard_type_ahead()` call from `around_a_question`. **1
    /// red of 872**, this test, on a tally that never moved — and *only* this
    /// test, which is the finding: the flush changes no byte any surface
    /// records and no answer any scripted prompter gives, so nothing else in
    /// the suite, and nothing a recorder can see at all, is in a position to
    /// notice it going missing. Reverted with the same edit.
    #[test]
    fn a_question_discards_the_kernels_type_ahead_before_it_is_drawn() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = turn_ctx!(surface, state, prompter);
        ctx.state.input.set_owned(true);
        ctx.state.input.push(b"half a thought");

        let before = crate::prompt::type_ahead_flushes();
        let (flushed, handed_over) = around_a_question(&mut ctx, |ctx| {
            (
                crate::prompt::type_ahead_flushes(),
                ctx.state.input.row(80, None),
            )
        });
        assert_eq!(
            flushed,
            before + 1,
            "the flush runs **before** the prompter is called, which is before \
             the question has drawn its own row"
        );
        assert_eq!(
            handed_over, None,
            "and the shelve it follows still happened: the editor the question \
             reads through is empty"
        );
        assert_eq!(
            crate::prompt::type_ahead_flushes(),
            before + 1,
            "once per question, not once per byte"
        );
    }

    /// The benign half of the rule above (LESSON-440): a question that opens
    /// while the pump does **not** own the input — the idle drain at a
    /// canonical entry prompt — flushes nothing, because the kernel's queue is
    /// then the user's own unsubmitted line. Mutation: dropping the
    /// `is_owned` gate reddens this test alone.
    #[test]
    fn a_question_outside_a_raw_turn_flushes_nothing() {
        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = turn_ctx!(surface, state, prompter);
        let before = crate::prompt::type_ahead_flushes();
        let seen = around_a_question(&mut ctx, |_| crate::prompt::type_ahead_flushes());
        assert_eq!(seen, before, "no flush inside the question");
        assert_eq!(
            crate::prompt::type_ahead_flushes(),
            before,
            "and none after it"
        );
    }

    /// **REQ-622, verify: one constructor and one fixture, one set of constants.**
    ///
    /// [`RowState::new`] and [`RowState::at_width`] restated all of the row's
    /// fields between them, which is a fixture free to disagree with the
    /// constructor it stands in for — and to disagree silently, since a fixture
    /// that started life believing a row was already on screen would simply
    /// skip the first draw. [`RowState::base`] is the one statement of the
    /// fields neither has an opinion about.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** set
    /// `pending_visible: true` in `base`. **27 red of 872** — this test plus
    /// twenty-six row and pump tests whose first paint then takes the repaint
    /// branch or opens with a withdraw. The blast radius is the argument for
    /// the helper: a constant that wrong is caught everywhere, and a constant
    /// *slightly* wrong in only one of two hand-written constructors is caught
    /// nowhere, because the fixture and the code it stands in for would agree
    /// with themselves. Reverted with the same edit.
    #[test]
    fn the_fixture_and_the_real_constructor_start_from_one_base() {
        let piped = RecordingSurface::new();
        let real = RowState::new(&piped);
        let fixture = RowState::at_width(80);

        for (what, row) in [("the constructor", &real), ("the fixture", &fixture)] {
            assert!(!row.activity_visible, "{what} starts with no activity row");
            assert!(!row.pending_visible, "{what} starts with no pending row");
            assert!(!row.owns_input(), "{what} starts without the terminal");
            assert!(
                row.input.guard.is_none(),
                "{what} starts holding no restore guard"
            );
            assert_eq!(row.tick, 0, "{what} starts at the first animation frame");
            assert!(!(row.line_waiting)(), "{what} sees no submitted line");
        }
        assert!(
            !real.live,
            "and the two differ in exactly what they are for: the constructor \
             takes the gate from the surface it was handed"
        );
        assert!(fixture.live, "the fixture claims a live surface");
    }

    /// **BR-14 / AC-12: Enter queues the line, the row says so, and nothing
    /// durable is printed.**
    ///
    /// The two halves are one rule. An Enter that registered has to be visible
    /// *before* the turn ends — otherwise the only feedback is the pending row
    /// disappearing, which is exactly what a swallowed keystroke looks like —
    /// and the queued line itself must not be printed, because the one place it
    /// is ever shown is where the next prompt echoes it (BR-4). So the pending
    /// row is withdrawn, the activity row gains a clause, and scrollback gains
    /// nothing.
    ///
    /// The second leg is the one an implementation reading only `Edit::Queued`
    /// gets wrong: the queue survives a turn by design (one line is drained per
    /// pass of the entry loop, ADR-622-5), so a line still waiting when the
    /// *next* turn opens has to be counted at the engage — `begin` has just
    /// cleared the figure with every other one a turn must not inherit.
    #[test]
    fn an_enter_queues_the_line_and_the_row_says_so() {
        script_keystrokes(&[b"ship it", b"\n"]);
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(3);
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> ship it".to_owned()),
                // Enter: the pending row is cleared where the cursor stands,
                // and the activity row — which was one above it all along —
                // says what happened.
                Rendered::WithdrawCurrent,
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ preparing turn · 0s · turn 0s · 1 queued".to_owned()
                ),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠹ preparing turn · 0s · turn 0s · 1 queued".to_owned()
                ),
                Rendered::Withdraw(1),
            ],
            "a queued line prints nothing durable: every line here is the \
             transient class"
        );
        assert_eq!(
            state.input.queued_len(),
            1,
            "the line is waiting to be sent, and the clause counted it"
        );

        // The second leg: a line still queued when the next turn opens.
        script_raw_mode(InputVerdict::Raw);
        let mut surface = RecordingSurface::with_live_rows();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = turn_ctx!(surface, state, prompter);
        let mut row = RowState::at_width(80);
        row.engage_input(&mut ctx, true);
        row.paint_rows(&mut ctx, Instant::now());

        assert!(row.owns_input(), "the fixture took the terminal");
        assert_eq!(
            surface.calls,
            vec![Rendered::Line(
                LineKind::Activity,
                "⠋ preparing turn · 0s · turn 0s · 1 queued".to_owned()
            )],
            "the line typed during the last turn is still waiting, so this \
             turn's first frame says so — `begin` cleared the count, and the \
             editor is what knows it was not zero"
        );
    }

    /// **BR-4 / BR-12: every way an `ENDS_TURN` call can return takes both rows
    /// back and puts the terminal back.**
    ///
    /// The three legs exit through **two different sites**, which is the point.
    /// A response — success or refusal — is a message, so the pump's
    /// withdraw-before-dispatch takes the block back on its way out. A dropped
    /// socket is a `?` out of the middle of the pump, and only `call`'s
    /// close-out runs on it: the block *and the raw-mode guard* are owned there
    /// and lent to the pump for exactly that reason (ADR-621-4, ADR-622-1). So
    /// termination depends on nothing the daemon sends.
    ///
    /// Two withdraws, bottom row first, on all three. That is BR-4 as
    /// scrollback: a turn the user typed into ends with the screen exactly as a
    /// turn they did not type into would — REQ-621's recorded exception, gone.
    ///
    /// The rows' text is left as `_` here and pinned literally in the tests
    /// above: this leg is about which verbs ran, and it drives the real `call`,
    /// whose width comes from the terminal the developer happens to be in.
    #[test]
    fn every_ends_turn_exit_withdraws_both_rows() {
        for (leg, reply) in [
            ("a normal result", Some(turn_answered(1))),
            ("an RpcError", Some(turn_refused(1))),
            ("a dropped socket", None),
        ] {
            script_raw_mode(InputVerdict::Raw);
            script_keystrokes(&[b"hi"]);
            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(1);
            match reply {
                Some(reply) => tx.send(reply).expect("queue"),
                // The reader thread is gone: `recv_timeout` reports
                // `Disconnected`, which is the same sentence `recv` produces.
                None => drop(tx),
            }

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut ctx = turn_ctx!(surface, state, prompter);
            let outcome = conn.call(turn_params(), &mut ctx);

            assert!(
                matches!(
                    surface.calls.as_slice(),
                    [
                        Rendered::Line(LineKind::Activity, _),
                        Rendered::DrawCurrent(LineKind::Pending, _),
                        Rendered::WithdrawCurrent,
                        Rendered::Withdraw(1),
                    ]
                ),
                "{leg} left a row behind: {:?}",
                surface.calls
            );
            assert!(
                state.last_turn_summary.is_some(),
                "{leg} must still report what the turn spent (BR-16)"
            );
            assert_eq!(
                state.activity.phase(),
                crate::activity::Phase::Idle,
                "{leg} must leave no turn in flight"
            );
            match leg {
                "a dropped socket" => assert_eq!(
                    outcome.expect_err("the connection is gone").to_string(),
                    "connection to the daemon closed",
                    "the timed receive reports a closed channel in the words \
                     the blocking one does"
                ),
                "a normal result" => assert!(outcome.expect("the response arrives").is_ok()),
                _ => assert!(outcome.expect("the response arrives").is_err()),
            }
        }
    }

    /// **BR-5: a question reads a fresh buffer, and the line under it comes
    /// back verbatim.**
    ///
    /// The older of the two hazards this REQ closes, and the one the activity
    /// row only made visible: a line typed while a tool ran sat in the kernel's
    /// buffer, and a permission prompt opened mid-turn was the next thing to
    /// read stdin — so the type-ahead was consumed as the answer to a question
    /// the user never saw. The fix is one seam ([`around_a_question`]) that
    /// shelves before the prompter is called and restores after.
    ///
    /// Three assertions, and the middle one is the only one that can see the
    /// bug. The **sequence** shows both rows down before the question prints
    /// and both back afterwards with the pending line intact. The **closure**
    /// leg observes the editor at the moment the prompter is called, which is
    /// the only moment at which a missing shelve is observable at all: a
    /// `Prompter` cannot reach the editor, so a question answered by the user's
    /// own pending line would render exactly the same bytes as one answered
    /// properly. The **structural** leg pins that all three arms that reach a
    /// prompter go through the seam, since the arm added next is the one that
    /// would not.
    ///
    /// **Mutation, applied and observed red (2026-09-10):** drop the
    /// `ctx.state.input.shelve()` from [`around_a_question`], so the question
    /// is asked over the user's own pending line. **1 red of 853**, this test,
    /// reporting `left: Some("> half a thought")` where the prompter must be
    /// handed nothing — at a real terminal that is a question answered by the
    /// first character of a sentence meant for the model, which is the whole of
    /// BR-5.
    ///
    /// That it reddens on the **closure leg and nowhere else** is the finding.
    /// The literal sequence above is **byte-identical** with the shelve gone:
    /// a `Prompter` is not a `Surface` and cannot reach the editor, so a
    /// question that ate the user's line renders exactly the same rows as one
    /// that did not — and the pty legs (TASK-419) see a terminal, which draws
    /// a stolen keystroke as willingly as a fresh one. A test written only
    /// against what was rendered could not have failed here (LESSON-569).
    /// Reverted with the same targeted edit; suite green at 853.
    #[test]
    fn a_question_reads_a_fresh_buffer_and_the_pending_line_comes_back() {
        script_keystrokes(&[b"why"]);
        let (mut conn, tx, _peer) = test_connection();
        conn.delay_replies_by_ticks(1);
        tx.send(wire_event(serde_json::json!({
            "session_id": "s1",
            "seq": 4,
            "event": "session_update",
            "update": {
                "kind": "tool_call",
                "tool_call_id": "c1",
                "title": "shell: cargo test",
                "status": "in_progress",
            },
        })))
        .expect("queue");
        tx.send(permission_envelope("shell")).expect("queue");
        tx.send(turn_answered(1)).expect("queue");

        let mut surface = RecordingSurface::with_live_rows();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::at_width(80).owning_input();
        let mut ctx = turn_ctx!(surface, state, prompter);
        conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
            .expect("the response arrives")
            .expect("the daemon answered");

        assert_eq!(
            surface.calls,
            vec![
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why".to_owned()),
                // The tool starts.
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
                Rendered::Line(
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why".to_owned()),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠙ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why".to_owned()),
                // The question. Nothing of either row is between the withdraws
                // and the question's own line: the prompt reaches a `Prompter`,
                // which writes straight to stdout and could not have taken a
                // row back for itself.
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
                Rendered::Line(
                    LineKind::Prompt,
                    "permission requested: shell — run `cargo test`".to_owned()
                ),
                // Answered — the row is back on the tool, and the half-written
                // line is back underneath it, exactly as it was.
                Rendered::Line(
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::DrawCurrent(LineKind::Pending, "> why".to_owned()),
                Rendered::Repaint(
                    1,
                    LineKind::Activity,
                    "⠹ running shell: cargo test · 0s · turn 0s".to_owned()
                ),
                Rendered::RepaintCurrent(LineKind::Pending, "> why".to_owned()),
                Rendered::WithdrawCurrent,
                Rendered::Withdraw(1),
            ]
        );

        // The seam itself, observed where a `Prompter` cannot look: the editor,
        // at the moment the question is asked. A fresh session, because the
        // turn above deliberately left its own half-typed line pending — the
        // queue and the pending line survive a turn (ADR-622-2), which is the
        // property that makes this leg worth writing on a state of its own.
        let mut surface = RecordingSurface::with_live_rows();
        let mut fresh = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = turn_ctx!(surface, fresh, prompter);
        ctx.state.input.push(b"half a thought");
        let seen = around_a_question(&mut ctx, |ctx| {
            // What the prompter is handed: an empty buffer. Anything typed from
            // here is the answer and nothing else is.
            let fresh = ctx.state.input.row(80, None);
            ctx.state.input.push(b"y");
            fresh
        });
        assert_eq!(
            seen, None,
            "the question reads a **fresh** buffer: with the pending line still \
             in it, the `y` above would have been appended to the user's own \
             sentence and the question answered by its first character"
        );
        assert_eq!(
            fresh.input.row(80, None).as_deref(),
            Some("> half a thought"),
            "and the line comes back verbatim, the answer's stray keystroke \
             discarded rather than merged"
        );

        // And every arm that reaches a `Prompter` goes through that seam. The
        // counts in `only_the_event_pump_declares_a_block_over` catch a fourth
        // arm appearing; this catches an existing one being written without it,
        // which is the direction a rule like this actually rots in.
        let client = crate::status::scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "client.rs")
            .map(|(_, src)| crate::status::scan::code_only(&src))
            .expect("client.rs is a production source");
        for (arm, callee, what) in [
            (
                "EventOutcome::Permission(req) if ctx.answer_permissions",
                "session_ui::resolve_permission(",
                "a permission question",
            ),
            (
                "EventOutcome::ModelProposal(proposal) if ctx.answer_model_proposals",
                "model_ui::resolve_proposal(",
                "a model proposal",
            ),
            (
                "EventOutcome::AttachConsent(request) if ctx.answer_permissions",
                "session_ui::resolve_attach_consent(",
                "an attach-consent question, which is an access-control \
                 decision as well as a question",
            ),
        ] {
            let at = client
                .find(arm)
                .unwrap_or_else(|| panic!("this sweep's anchor is gone from client.rs: {arm:?}"));
            let len = client[at..]
                .find(callee)
                .unwrap_or_else(|| panic!("{callee:?} no longer follows {arm:?}"));
            assert!(
                client[at..at + len].contains("around_a_question("),
                "{what} must read only what was typed after it was drawn \
                 (BR-5), and the pending line must survive it — so it goes \
                 through the one seam that shelves and restores, not past it"
            );
        }
    }

    /// **BR-11 / AC-11: a terminal that refuses the mode change falls back to
    /// last month's behaviour, and says so once.**
    ///
    /// Fail **open**, which is the opposite polarity from the echo-off key
    /// prompt a few lines away in `prompt.rs`, and the asymmetry is the point:
    /// there, a terminal that will not stop painting would paint a credential
    /// into scrollback, so failing to hide means not reading. Here the bytes at
    /// stake are the user's own prompt on their own screen. The fallback is not
    /// a leak but REQ-621 exactly — the kernel assembles the line, the terminal
    /// echoes it, and a submitted line gives up the row rather than painting
    /// over it — so refusing the turn would trade a cosmetic regression for a
    /// dead CLI.
    ///
    /// Both halves of the rule, in both polarities of `verbose`. A row
    /// abandoned in silence is indistinguishable from a daemon that has gone
    /// quiet, which is the ambiguity BR-11's stall annotation exists to remove;
    /// and a notice at every frame would be the same failure by volume.
    #[test]
    fn a_refused_raw_mode_falls_back_to_abandon_and_says_so() {
        for verbose in [false, true] {
            script_raw_mode(InputVerdict::Failed);
            // Keystrokes that must never be read: on the canonical path the
            // kernel owns the line, and a pump reading bytes as well would take
            // half of one.
            script_keystrokes(&[b"typed anyway"]);

            let mut surface = RecordingSurface::with_live_rows();
            let mut state = SessionState::new();
            state.verbose = verbose;
            let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
            let mut row = RowState::at_width(80);
            let mut ctx = turn_ctx!(surface, state, prompter);
            row.engage_input(&mut ctx, true);

            assert_eq!(
                engage_attempts(),
                1,
                "the terminal was asked (verbose {verbose})"
            );
            assert!(
                !row.owns_input() && row.input.guard.is_none(),
                "and refused, so this call owns neither the input nor a guard \
                 (verbose {verbose})"
            );

            // A row on screen, drawn the way the pump draws one, and then the
            // user submits a line — REQ-621's path, still armed.
            row.paint_rows(&mut ctx, Instant::now());
            assert!(
                row.activity_visible && !row.pending_visible,
                "one row, not two: the pending row belongs to a client that \
                 owns the input (verbose {verbose})"
            );
            row.line_waiting = || true;

            let (mut conn, tx, _peer) = test_connection();
            conn.delay_replies_by_ticks(2);
            tx.send(wire_event(serde_json::json!({
                "session_id": "s1",
                "seq": 3,
                "event": "session_update",
                "update": {
                    "kind": "tool_call",
                    "tool_call_id": "c1",
                    "title": "shell: cargo test",
                    "status": "in_progress",
                },
            })))
            .expect("queue");
            tx.send(turn_answered(1)).expect("queue");
            conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
                .expect("a refused mode change must not fail the turn")
                .expect("the daemon answered");

            let mut expected = Vec::new();
            if verbose {
                expected.push(Rendered::Line(LineKind::Info, RAW_REFUSED.to_owned()));
            }
            expected.extend([
                // The frame that was already on screen when Enter was pressed.
                // It stays there: REQ-621 BR-5's recorded exception, which
                // survives on exactly this path.
                Rendered::Line(
                    LineKind::Activity,
                    "⠋ preparing turn · 0s · turn 0s".to_owned(),
                ),
                // And the turn goes on writing what belongs in the log.
                Rendered::Line(LineKind::Tool, "shell: cargo test [running]".to_owned()),
            ]);
            assert_eq!(
                surface.calls, expected,
                "one notice and nothing else new: no pending row, and no \
                 repaint or withdraw after a submitted line (verbose {verbose})"
            );
            assert!(
                !row.live && !row.activity_visible && !row.pending_visible,
                "the rows are given up for the rest of the turn (verbose \
                 {verbose})"
            );
            assert!(
                state.input.row(80, None).is_none(),
                "and nothing was read: the kernel is still assembling the line \
                 (verbose {verbose})"
            );
        }
    }

    /// **AC-5.** Every event the row consumes moves the phase, driven through
    /// the real pump and the real `dispatch_event`.
    ///
    /// **Each envelope is the daemon's own serialization of the daemon's own
    /// type.** `published` below builds `EventEnvelope::new(seq, session,
    /// Event::X(..))` — the exact call `tetond`'s bus makes
    /// (`broadcast.rs`) — from the protocol's typed payload structs, and then
    /// serializes it. So a payload that changed shape cannot leave this fixture
    /// describing a value nothing produces: a field renamed on the wire is
    /// renamed in the fixture with it, and a field the daemon stopped sending
    /// stops arriving here. Hand-written `json!` literals were what this test
    /// carried until REQ-621's verify pass, and they are the LESSON-544 shape
    /// exactly — a `"tier"` key that quietly became `"tier_band"` would have
    /// left every literal deserializing with no tier at all, the row losing the
    /// only name BR-2 lets it print, and this test green throughout.
    ///
    /// It is deliberately still a **serialization**, deserialized back through
    /// `wire_event`, rather than a typed envelope handed straight to the pump:
    /// the client's real input is bytes, and the round trip is the seam.
    ///
    /// Each step is a whole `pump_until_answered` — the send, the receive, the
    /// fold, the dispatch, the response — so the *order* the pump does those
    /// things in is under test too: fold before render, which is what keeps the
    /// row and the durable line beside it describing one moment.
    ///
    /// **Which of these events a real daemon also drives into the row**, since
    /// this test is the whole of AC-5 for four of them. The pty legs
    /// (`pty_e2e.rs`) run the scripted engine against the real daemon and reach
    /// `route_decided`, `tool_call`, `tool_call_update` and `cost_recorded` that
    /// way. They cannot reach `prefill_progress` (no local model loads under
    /// the scripted engine), `context_compacted` (no context grows large
    /// enough), `turn_queued` (no tier warms) or `permission_request` (the
    /// fixtures grant at `full` so nothing asks) — so for those four this is the
    /// only test that drives the phase from the producer's own payload, which is
    /// why the fixture had to stop being a literal.
    ///
    /// **Mutation, applied and observed (2026-09-10):** add a field the daemon
    /// publishes to a payload the row reads — `#[serde(default)] pub attempt:
    /// u32` on `RouteDecided`. The fixture below **stops compiling** (`E0063:
    /// missing field attempt`, at this test and at five sibling payload
    /// constructions), which is a payload that changed shape failing at the
    /// fixture, loudly. The `json!` literal it replaced named five of
    /// `RouteDecided`'s fourteen fields and would have gone on compiling and
    /// passing: serde fills a defaulted field from nothing, and this test
    /// asserts phases, so the row quietly losing a field it is allowed to print
    /// is invisible to it. That asymmetry is the finding. Reverted with the
    /// same edit.
    ///
    /// The permission step asserts the restore rather than the phase, because
    /// the pump answers the question inside the same step: what the row must do
    /// is come back to the sentence it left (BR-1), and the phase it left is the
    /// only thing that says which.
    #[test]
    fn phases_follow_events_through_the_real_dispatch() {
        use crate::activity::Phase;
        use teton_protocol::events::{self, Event};
        use teton_protocol::{ProviderId, Tier};

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut row = RowState::new(&surface);
        let mut ctx = turn_ctx!(surface, state, prompter);

        assert_eq!(
            ctx.state.activity.phase(),
            Phase::Preparing,
            "the prompt is on the wire and the daemon has not answered yet"
        );

        let script: Vec<(&str, Value, Phase)> = vec![
            (
                "route_decided",
                published(
                    1,
                    "s1",
                    Event::RouteDecided(events::RouteDecided {
                        category: None,
                        tier: Some(Tier::Think),
                        phase: None,
                        provider_id: ProviderId::from("anthropic"),
                        model: Some("claude-opus-5".to_owned()),
                        reason: "fixture".to_owned(),
                        effort: None,
                        window_tokens: None,
                        budget_tokens: None,
                        budget_bytes: None,
                        bound: None,
                        spend_ceiling_micro_cents: None,
                        bound_floored: None,
                        repo_context_cap: None,
                    }),
                ),
                Phase::AwaitingModel,
            ),
            (
                // The daemon's own fraction: the phase is `awaiting_model`
                // making progress, not a phase of its own (BR-3).
                "prefill_progress",
                published(
                    2,
                    "s1",
                    Event::PrefillProgress(events::PrefillProgress {
                        tokens_done: 4_096,
                        tokens_total: 32_768,
                        tokens_per_second: 512.0,
                    }),
                ),
                Phase::AwaitingModel,
            ),
            (
                // Being billed is not a thing the turn is *doing*.
                "cost_recorded",
                published(
                    3,
                    "s1",
                    Event::CostRecorded(events::CostRecorded {
                        record: events::CostRecord {
                            session_id: teton_protocol::SessionId::from("s1"),
                            phase: None,
                            category: None,
                            provider_id: ProviderId::from("anthropic"),
                            model: "claude-opus-5".to_owned(),
                            input_tokens: 100,
                            output_tokens: 50,
                            usd_micros: 12_345,
                            cached_tokens: None,
                            reasoning_tokens: None,
                            probe: false,
                        },
                    }),
                ),
                Phase::AwaitingModel,
            ),
            (
                "agent_message_chunk",
                published(
                    4,
                    "s1",
                    Event::SessionUpdate(events::SessionUpdate {
                        update: events::SessionUpdatePayload::AgentMessageChunk {
                            text: "the finding is".to_owned(),
                        },
                    }),
                ),
                Phase::Streaming,
            ),
            (
                "tool_call",
                published(
                    5,
                    "s1",
                    Event::SessionUpdate(events::SessionUpdate {
                        update: events::SessionUpdatePayload::ToolCall {
                            tool_call_id: "c1".to_owned(),
                            title: "shell: cargo test".to_owned(),
                            status: events::ToolCallStatus::InProgress,
                        },
                    }),
                ),
                Phase::ToolRunning,
            ),
            (
                // A finished tool means the model is composing its next step;
                // there is no "model request issued" event, and BR-14 needed
                // none.
                "tool_call_update",
                published(
                    6,
                    "s1",
                    Event::SessionUpdate(events::SessionUpdate {
                        update: events::SessionUpdatePayload::ToolCallUpdate {
                            tool_call_id: "c1".to_owned(),
                            status: events::ToolCallStatus::Completed,
                        },
                    }),
                ),
                Phase::AwaitingModel,
            ),
            (
                "context_compacted",
                published(
                    7,
                    "s1",
                    Event::ContextCompacted(events::ContextCompacted {
                        kept_bytes: 4_096,
                        dropped_bytes: 2_048,
                        summarized_bytes: 512,
                        anchor_bytes: 128,
                        dropped_blocks: Vec::new(),
                        dropped_blocks_omitted: 0,
                        provider_id: Some("anthropic".to_owned()),
                        fallback: false,
                    }),
                ),
                Phase::Compacting,
            ),
            (
                "turn_queued",
                published(
                    8,
                    "s1",
                    Event::TurnQueued(events::TurnQueued {
                        turn_id: teton_protocol::TurnId::from("t1"),
                        model_id: "qwen3-coder-30b-a3b".to_owned(),
                        waiting_on: events::TierWarming::Loading,
                    }),
                ),
                Phase::Held,
            ),
            (
                // BR-1: the question owns the terminal, the pump answers it,
                // and the phase it interrupted comes back.
                "permission_request",
                published(
                    9,
                    "s1",
                    Event::PermissionRequest(events::PermissionRequest {
                        request_id: teton_protocol::RequestId::from("r1"),
                        tool_name: "shell".to_owned(),
                        description: None,
                        subject: None,
                        options: vec![events::PermissionOption {
                            option_id: "reject_once".to_owned(),
                            label: "Reject once".to_owned(),
                            kind: events::PermissionOptionKind::RejectOnce,
                        }],
                    }),
                ),
                Phase::Held,
            ),
            (
                // BR-15 / AC-9: another session's event on the daemon-wide bus
                // changes nothing here.
                "another session's chunk",
                published(
                    10,
                    "s2",
                    Event::SessionUpdate(events::SessionUpdate {
                        update: events::SessionUpdatePayload::AgentMessageChunk {
                            text: "not ours".to_owned(),
                        },
                    }),
                ),
                Phase::Held,
            ),
        ];

        // One connection per step, so each step's reply is request id 1.
        // Correlation is the connection's own counter, and the permission arm
        // spends one of its ids answering the question — a fixture that derived
        // the id from the step index would hand step ten a reply for a request
        // the pump had not made, and the pump would sit waiting for one that
        // never comes. The phase lives on the state, which outlives them all.
        for (what, json, expected) in script {
            let (mut conn, tx, _peer) = test_connection();
            tx.send(wire_event(json)).expect("queue");
            tx.send(turn_answered(1)).expect("queue");
            conn.pump_until_answered(turn_params(), &mut ctx, &mut row)
                .expect("the response arrives")
                .expect("the daemon answered");
            assert_eq!(
                ctx.state.activity.phase(),
                expected,
                "the phase after {what} came through the pump"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// REQ-585 ADR-2: `skills/list` is the version handshake
// ---------------------------------------------------------------------------

#[cfg(test)]
mod skills_handshake {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use crate::slash::{self, SkillSnapshot};
    use teton_protocol::methods::{SkillSource, SkillView, SkillsListResult};
    use teton_protocol::SessionId;

    fn session() -> SessionId {
        SessionId::from("s1")
    }

    /// A context in a session, at a terminal — the launch shape.
    macro_rules! ctx {
        ($surface:ident, $state:ident, $prompter:ident, $session:expr) => {
            UiContext {
                surface: &mut $surface,
                state: &mut $state,
                prompter: &mut $prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
                session_id: $session,
                skills: SkillSnapshot::empty(),
            }
        };
    }

    fn served(skills: Vec<SkillView>) -> Value {
        serde_json::to_value(SkillsListResult {
            skills,
            skipped: Vec::new(),
        })
        .expect("the result serializes")
    }

    fn skill(name: &str) -> SkillView {
        SkillView {
            name: name.to_owned(),
            source: SkillSource::User,
            description: Some("Report on the repo.".to_owned()),
            argument_hint: None,
            shadowed: None,
            // The ordinary row, which after REQ-587 BR-3 is invocable from both
            // doors: these tests are about a served registry becoming the
            // session's snapshot, and the fixture should be the case they are
            // named for. `model_invocable: false` would describe a skill absent
            // from the model's roster, which is a state worth a fixture of its
            // own where a mark or a roster is under test — `slash.rs` has those
            // (`user_only`, `model_only`, `invocable_by_nobody`) — and is a
            // misleading default here.
            model_invocable: true,
            user_invocable: true,
        }
    }

    /// The registry the daemon serves becomes the session's snapshot, and the
    /// request it answers is the session-scoped `skills/list` — not a second
    /// reader of `~/.claude` in this crate (ADR-1).
    #[test]
    fn a_served_registry_becomes_the_sessions_snapshot() {
        let (mut conn, peer) = Connection::scripted(&[served(vec![skill("status")])]);
        let mut surface = RecordingSurface::new();
        let mut state = session_ui::SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut ctx = ctx!(surface, state, prompter, Some(session()));

        conn.refresh_skills(&mut ctx).expect("the daemon answered");

        assert_eq!(methods_written(&peer), vec!["skills/list".to_owned()]);
        assert!(
            matches!(
                slash::classify("/status now", &ctx.skills),
                slash::Input::Skill { ref name, .. } if name == "status"
            ),
            "the snapshot is what `classify` dispatches from"
        );
        conn.assert_all_consumed();
    }

    /// **ADR-2, the whole mechanism.** A daemon that does not serve
    /// `skills/list` answers `METHOD_NOT_FOUND`, and that is **an empty
    /// registry, not an error**: the session starts, `/status` classifies as
    /// the unknown command it has always been, and `PromptTurnParams.skill` is
    /// therefore never sent to a daemon that could not understand it. The new
    /// consent can never arrive from it either, because nothing on that daemon
    /// ever raises one.
    ///
    /// Treating the absent method as a failure fails here — and would have
    /// turned "your daemon is a version behind" into "your session will not
    /// start".
    #[test]
    fn an_old_daemon_leaves_an_empty_snapshot_and_raises_no_error() {
        let (mut conn, _peer) = Connection::scripted_replies(vec![Err(RpcError::new(
            error_code::METHOD_NOT_FOUND,
            "Method not found",
        ))]);
        let mut surface = RecordingSurface::new();
        let mut state = session_ui::SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut ctx = ctx!(surface, state, prompter, Some(session()));

        conn.refresh_skills(&mut ctx)
            .expect("an absent method is not an error");

        assert_eq!(ctx.skills, SkillSnapshot::empty());
        assert!(
            surface.calls.is_empty(),
            "and it says nothing: a registry nobody could produce is a session \
             with no skills, not news: {:?}",
            surface.calls
        );
    }

    /// The composition ADR-2 actually buys, asserted end to end rather than
    /// inferred: with the empty snapshot an old daemon leaves, no `/` line can
    /// classify as a skill, so nothing on this path can build a
    /// `PromptTurnParams` carrying one.
    #[test]
    fn an_empty_snapshot_classifies_no_skill_so_no_skill_field_is_ever_sent() {
        let empty = SkillSnapshot::empty();
        for line in [
            "/status",
            "/status REQ-585",
            "/analyze the repo",
            "/proceed",
        ] {
            assert!(
                !matches!(slash::classify(line, &empty), slash::Input::Skill { .. }),
                "{line:?} must not dispatch a skill against an old daemon"
            );
        }
        // And the only turn builder reachable from those classifications leaves
        // the field absent, which is what keeps the request byte-identical to a
        // pre-REQ-585 one.
        assert!(slash::prompt_turn_params(&session(), "/status")
            .skill
            .is_none());
    }

    /// No session, no registry: `skills/list` is session-scoped, so a passive
    /// context — or the window before `session/create` answers — asks nothing
    /// at all rather than asking about a session it does not have.
    #[test]
    fn a_context_with_no_session_asks_for_no_registry() {
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = session_ui::SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut ctx = ctx!(surface, state, prompter, None);

        conn.refresh_skills(&mut ctx).expect("nothing to ask");

        assert!(methods_written(&peer).is_empty());
        assert_eq!(ctx.skills, SkillSnapshot::empty());
    }

    /// Any other refusal reads the same way, and for the same reason: a session
    /// keeps running with no skills rather than failing to start over a
    /// registry.
    #[test]
    fn a_refused_query_leaves_an_empty_snapshot_too() {
        let snapshot = snapshot_from_skills_reply(Err(RpcError::new(
            error_code::INTERNAL_ERROR,
            "the registry could not be read",
        )));
        assert_eq!(snapshot, SkillSnapshot::empty());
    }
}
