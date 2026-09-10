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
use crate::model_ui;
use crate::prompt::Prompter;
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
    /// Whether the row is on screen right now, one row above the cursor.
    visible: bool,
    /// Which animation frame the row is showing. Advanced by a `Tick` only: a
    /// redraw after a durable line is not an animation step.
    tick: u64,
    /// The surface width the row is fitted to, read once per call beside
    /// [`Self::live`].
    ///
    /// A turn is a few seconds and a resize mid-turn is rare; re-reading it per
    /// frame would be a `TIOCGWINSZ` eight times a second for the whole of
    /// every turn, and the row is one line whose only failure on a stale width
    /// is a truncation a character early.
    width: usize,
}

impl RowState {
    /// The row's state at the start of a call: nothing drawn, gate and width
    /// taken from the surface.
    fn new(surface: &dyn Surface) -> Self {
        let live = surface.has_live_rows();
        Self {
            live,
            visible: false,
            tick: 0,
            // Asked only when there is a row to fit. The query is an `ioctl` on
            // `STDOUT_FILENO`, and every non-turn RPC comes through here too —
            // some thirty of them — with nothing to measure.
            width: if live {
                crate::prompt::terminal_width()
            } else {
                0
            },
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
        Self {
            live: true,
            visible: false,
            tick: 0,
            width,
        }
    }
}

/// Make what is on screen match the frame the projection would draw now
/// (REQ-621 ADR-621-3).
///
/// The row's entire discipline, in one place so the tick arm and the
/// after-a-message redraw cannot come to disagree:
///
/// | frame | on screen | done |
/// |---|---|---|
/// | `Some` | nothing | `line` — the row scrolls in beneath whatever was last written |
/// | `Some` | the row | `repaint_row_above(1, ..)` — in place, so it never accumulates (BR-5) |
/// | `None` | the row | `withdraw_row_above(1)` — gone without residue, not blanked |
/// | `None` | nothing | nothing at all |
///
/// `repaint_row_above` and `withdraw_row_above` measure from the cursor, and
/// after `line()` drew the row the cursor is on the row below it — so the row
/// is always exactly one up while it is visible.
///
/// Gated on [`RowState::live`], so a piped surface reaches no verb here at all
/// (BR-6). Whether there is a row is then the **projection's** decision and
/// never the method's: a `/cost` pumping through this loop finds the activity
/// idle and paints nothing (ADR-621-1).
fn paint_row(row: &mut RowState, ctx: &mut UiContext, now: Instant) {
    if !row.live {
        return;
    }
    match ctx.state.activity.frame(now, row.tick, row.width) {
        Some(text) if row.visible => {
            ctx.surface.repaint_row_above(1, LineKind::Activity, &text);
        }
        Some(text) => {
            ctx.surface.line(LineKind::Activity, &text);
            row.visible = true;
        }
        None if row.visible => {
            ctx.surface.withdraw_row_above(1);
            row.visible = false;
        }
        None => {}
    }
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
        let mut row = RowState::new(ctx.surface);
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
        if row.visible {
            ctx.surface.withdraw_row_above(1);
        }
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
            let message = match wake {
                // The daemon said nothing for a frame. Advance the animation
                // *after* the paint, so a row's first frame is the cycle's
                // first frame and a redraw between ticks shows the frame the
                // last tick drew.
                Wake::Tick => {
                    paint_row(row, ctx, Instant::now());
                    row.tick = row.tick.wrapping_add(1);
                    continue;
                }
                Wake::Message(message) => message,
            };
            // **Withdraw before anything else writes** — the row's whole
            // discipline (ADR-621-3), and it is here rather than inside
            // `dispatch_event` on purpose: the idle drain shares that function
            // and must never touch a row it does not own. A durable line —
            // `shell: … [running]`, a notice, a permission question — therefore
            // always prints where the row was, and the row comes back beneath
            // it a few lines below.
            if row.visible {
                ctx.surface.withdraw_row_above(1);
                row.visible = false;
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
                    paint_row(row, ctx, Instant::now());
                }
                Incoming::Lagged(err) => {
                    report_lag(&err, ctx.surface);
                    paint_row(row, ctx, Instant::now());
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
                let reply = session_ui::resolve_permission(
                    &req,
                    &mut *ctx.surface,
                    &mut *ctx.prompter,
                    &mut ctx.state.grants,
                    // REQ-585 BR-11 / ADR-8. The terminal fact is threaded from
                    // the one edge that read it (`main.rs`'s `IsTerminal` on
                    // stdin), never recomputed inside the UI: a handler reading
                    // `std::io::stdin()` itself would be a second, invisible
                    // seam, and the gate this feeds is precisely the one that
                    // must not be answerable differently in two places.
                    ctx.typed_input,
                );
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
                    if let Some(reply) = model_ui::resolve_proposal(
                        &proposal,
                        ctx.auto_accept_model,
                        &mut *ctx.surface,
                        &mut *ctx.prompter,
                    ) {
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
                let reply = session_ui::resolve_attach_consent(
                    &request,
                    &mut *ctx.surface,
                    &mut *ctx.prompter,
                );
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
/// Daemon-scoped envelopes (`None` — model download progress, daemon lifetime)
/// always render, including while `ours` is still `None`: the window before
/// `session/create` answers is exactly where first-run consent speaks, and a
/// client that went quiet there would download 18 GiB in silence. A
/// session-scoped envelope renders only when it names our session, so a client
/// without one renders nothing session-scoped — none of it is its own.
fn should_render(
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
        assert!(!row.visible, "the row was withdrawn for the stream");
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
        assert!(!row.visible);
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
        assert!(!row.visible);
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

    /// **AC-5.** Every event the row consumes moves the phase, driven through
    /// the real pump and the real `dispatch_event`.
    ///
    /// The envelopes are deserialized from the wire JSON the daemon's own
    /// publisher emits, not built as struct literals, so a payload that changed
    /// shape fails here rather than passing against a value nothing produces
    /// (LESSON-544). Each step is a whole `pump_until_answered` — the send, the
    /// receive, the fold, the dispatch, the response — so the *order* the pump
    /// does those things in is under test too: fold before render, which is what
    /// keeps the row and the durable line beside it describing one moment.
    ///
    /// The permission step asserts the restore rather than the phase, because
    /// the pump answers the question inside the same step: what the row must do
    /// is come back to the sentence it left (BR-1), and the phase it left is the
    /// only thing that says which.
    #[test]
    fn phases_follow_events_through_the_real_dispatch() {
        use crate::activity::Phase;

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
                serde_json::json!({
                    "session_id": "s1", "seq": 1, "event": "route_decided",
                    "provider_id": "anthropic", "model": "claude-opus-5",
                    "tier": "think", "reason": "fixture",
                }),
                Phase::AwaitingModel,
            ),
            (
                // The daemon's own fraction: the phase is `awaiting_model`
                // making progress, not a phase of its own (BR-3).
                "prefill_progress",
                serde_json::json!({
                    "session_id": "s1", "seq": 2, "event": "prefill_progress",
                    "tokens_done": 4096, "tokens_total": 32768,
                    "tokens_per_second": 512.0,
                }),
                Phase::AwaitingModel,
            ),
            (
                // Being billed is not a thing the turn is *doing*.
                "cost_recorded",
                serde_json::json!({
                    "session_id": "s1", "seq": 3, "event": "cost_recorded",
                    "record": {
                        "session_id": "s1", "provider_id": "anthropic",
                        "model": "claude-opus-5", "input_tokens": 100,
                        "output_tokens": 50, "usd_micros": 12_345,
                    },
                }),
                Phase::AwaitingModel,
            ),
            (
                "agent_message_chunk",
                serde_json::json!({
                    "session_id": "s1", "seq": 4, "event": "session_update",
                    "update": { "kind": "agent_message_chunk", "text": "the finding is" },
                }),
                Phase::Streaming,
            ),
            (
                "tool_call",
                serde_json::json!({
                    "session_id": "s1", "seq": 5, "event": "session_update",
                    "update": {
                        "kind": "tool_call", "tool_call_id": "c1",
                        "title": "shell: cargo test", "status": "in_progress",
                    },
                }),
                Phase::ToolRunning,
            ),
            (
                // A finished tool means the model is composing its next step;
                // there is no "model request issued" event, and BR-14 needed
                // none.
                "tool_call_update",
                serde_json::json!({
                    "session_id": "s1", "seq": 6, "event": "session_update",
                    "update": {
                        "kind": "tool_call_update", "tool_call_id": "c1",
                        "status": "completed",
                    },
                }),
                Phase::AwaitingModel,
            ),
            (
                "context_compacted",
                serde_json::json!({
                    "session_id": "s1", "seq": 7, "event": "context_compacted",
                    "kept_bytes": 4096, "dropped_bytes": 2048,
                    "summarized_bytes": 512, "anchor_bytes": 128,
                    "dropped_blocks": [], "provider_id": "anthropic",
                    "fallback": false,
                }),
                Phase::Compacting,
            ),
            (
                "turn_queued",
                serde_json::json!({
                    "session_id": "s1", "seq": 8, "event": "turn_queued",
                    "turn_id": "t1", "model_id": "qwen3-coder-30b-a3b",
                    "waiting_on": "loading",
                }),
                Phase::Held,
            ),
            (
                // BR-1: the question owns the terminal, the pump answers it,
                // and the phase it interrupted comes back.
                "permission_request",
                serde_json::json!({
                    "session_id": "s1", "seq": 9, "event": "permission_request",
                    "request_id": "r1", "tool_name": "shell",
                    "options": [{
                        "option_id": "reject_once", "label": "Reject once",
                        "kind": "reject_once",
                    }],
                }),
                Phase::Held,
            ),
            (
                // BR-15 / AC-9: another session's event on the daemon-wide bus
                // changes nothing here.
                "another session's chunk",
                serde_json::json!({
                    "session_id": "s2", "seq": 10, "event": "session_update",
                    "update": { "kind": "agent_message_chunk", "text": "not ours" },
                }),
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
