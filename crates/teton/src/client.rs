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
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

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
        let id = self.send(params)?;
        loop {
            match self.recv()? {
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
                    self.dispatch_event(&env, ctx, &mut || {})?;
                }
                Incoming::Lagged(err) => report_lag(&err, ctx.surface),
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
                    let rendered = self.dispatch_event(&env, ctx, &mut || {
                        if is_first {
                            on_first();
                        }
                    })?;
                    if rendered {
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
    /// counts the ones that happened.
    fn dispatch_event(
        &mut self,
        env: &teton_protocol::events::EventEnvelope,
        ctx: &mut UiContext,
        before_render: &mut dyn FnMut(),
    ) -> anyhow::Result<bool> {
        // REQ-568 AC-8: defense in depth atop the daemon-side filter (BR-3),
        // never a substitute for it — the daemon decides who may *see* a
        // session's events, and it is the only place that decision is a control.
        // This drop only keeps a stale or second daemon from painting somebody
        // else's session onto this screen.
        if !should_render(env.session_id.as_ref(), ctx.session_id.as_ref()) {
            return Ok(false);
        }
        before_render();
        match session_ui::render_event(env, &mut *ctx.surface, &mut *ctx.state) {
            EventOutcome::Rendered => {}
            EventOutcome::Permission(req) if ctx.answer_permissions => {
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
        Ok(true)
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

    use crate::render::RecordingSurface;

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
