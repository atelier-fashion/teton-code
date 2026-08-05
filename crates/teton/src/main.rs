//! teton — the Teton Code CLI.
//!
//! A thin client (engine/surface separation, BR-4): it renders the daemon's
//! event stream and forwards input over the bespoke JSON-RPC protocol (ADR-002).
//! The default invocation opens an interactive freeform session; subcommands
//! manage providers, privacy boundaries, the routing policy, the cost meter, and
//! diagnostics.
//!
//! All differentiating logic lives in `tetond`; this binary only speaks the
//! protocol and paints results through the [`render::Surface`] seam. It holds no
//! HTTP client of its own — every remote call is the daemon's, through its single
//! egress choke point (BR-1).

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    ConfigGetParams, ConfigSetParams, ConfigUpdate, CostQueryParams, CostQueryResult,
    CostReportView, ModelListParams, ModelListResult, ModelSetParams, ModelStatusParams,
    ModelStatusResult, PrivacyBoundaryConfig, ProviderConfig, RoutingRule, SessionCreateParams,
};
use teton_protocol::{Phase, PrivacyMode, ProviderId, ProviderKind, SessionMode};

mod banner;
mod client;
mod cost_ui;
mod firstrun;
mod keychain;
mod loading;
mod model_ui;
mod prompt;
mod render;
mod service;
mod session_ui;
mod slash;
mod uninstall;

use client::{Connection, UiContext};
use keychain::Keychain;
use prompt::{FramedStdinPrompter, Prompter, StdinPrompter};
use render::{stdout_surface, LineKind, Surface};
use session_ui::SessionState;
use teton_protocol::socket_path::{self, DaemonPaths};

/// The `teton` command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "teton",
    version,
    about = "Teton Code — hybrid local/remote AI coding agent with workflow-aware routing",
    long_about = None,
)]
struct Cli {
    /// Answer the first-run local-model prompt with "accept" and read no input
    /// (REQ-547 BR-5): the explicit opt-in for unattended/CI runs. Also supplies
    /// the second confirmation `teton model set` needs for a model above this
    /// machine's RAM floor (BR-3), the same confirmation for the in-session
    /// `/model set <name>` (REQ-555 BR-4b — one flow, so the session inherits
    /// the flag as the explicit unattended stand-in and consumes no input line
    /// for the question), and the deletion confirmation of `teton uninstall`.
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Show routing and turn-end notices in the interactive session. By default
    /// the transcript is just the conversation — model responses and tool
    /// activity; privacy and degradation warnings always show.
    #[arg(long, short = 'v', global = true)]
    verbose: bool,

    /// The subcommand to run; omit to open an interactive session.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Manage model providers (Anthropic, OpenAI-compatible, local).
    Provider {
        /// The provider action.
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Manage privacy boundaries (paths that never leave the machine).
    Boundary {
        /// The boundary action.
        #[command(subcommand)]
        action: BoundaryAction,
    },
    /// Inspect or set the workflow-aware routing policy.
    Policy {
        /// The policy action.
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Inspect and change the local model (AC-9).
    Model {
        /// The model action.
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Show the cost meter: total, per-phase attribution, and savings estimate.
    Cost,
    /// Diagnose the daemon, socket, model state, and providers.
    Doctor,
    /// Remove Teton Code from this machine: stop the daemon, delete its data
    /// and logs, and `brew uninstall` the binaries — the whole chain, confirmed
    /// up front.
    Uninstall {
        /// Keep the state directory (the downloaded model, cost history, and
        /// config); remove only the service, logs, and binaries.
        #[arg(long)]
        keep_data: bool,
    },
}

/// `teton model …` (AC-9)
#[derive(Debug, Subcommand)]
enum ModelAction {
    /// Show the catalog, each entry's fit for this machine, and the selection.
    List,
    /// Change the selected model. A model above this machine's RAM floor needs a
    /// second confirmation (BR-3) — interactively, or with `--yes`.
    Set {
        /// Catalog name to switch to (see `teton model list`).
        name: String,
    },
    /// Report the recorded decision and the weights' install state.
    Status,
}

/// `teton provider …`
#[derive(Debug, Subcommand)]
enum ProviderAction {
    /// Register a provider; its key is stored in the OS keychain (BR-7).
    Add {
        /// Provider id (e.g. `anthropic`, `deepseek`).
        id: String,
        /// Provider family.
        #[arg(long, value_enum)]
        kind: CliProviderKind,
        /// Endpoint URL (required for remote kinds).
        #[arg(long)]
        endpoint: Option<String>,
        /// The model this provider calls, e.g. `claude-opus-5` (REQ-557 BR-1).
        /// Required for remote kinds; never inferred from the provider id.
        #[arg(long)]
        model: Option<String>,
    },
    /// List configured providers.
    List,
}

/// `teton boundary …`
#[derive(Debug, Subcommand)]
enum BoundaryAction {
    /// Add a privacy boundary over a repo-relative path glob.
    Add {
        /// Repo-relative glob the boundary applies to.
        glob: String,
        /// Enforcement mode.
        #[arg(long, value_enum, default_value = "local-only")]
        mode: CliPrivacyMode,
    },
    /// List configured privacy boundaries.
    List,
}

/// `teton policy …`
#[derive(Debug, Subcommand)]
enum PolicyAction {
    /// Route a phase to a provider (with an optional fallback).
    Set {
        /// The lifecycle phase to route.
        #[arg(value_enum)]
        phase: CliPhase,
        /// Provider id to route the phase to.
        provider: String,
        /// Provider used on error/timeout of the primary.
        #[arg(long)]
        fallback: Option<String>,
    },
    /// Show the current routing policy.
    Show,
}

/// CLI mirror of [`ProviderKind`] (kebab-case wire names).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliProviderKind {
    /// On-device local tier.
    Local,
    /// Any OpenAI-compatible endpoint.
    OpenaiCompatible,
    /// Anthropic Messages API.
    Anthropic,
    /// Bespoke integration.
    Custom,
}

impl From<CliProviderKind> for ProviderKind {
    fn from(kind: CliProviderKind) -> Self {
        match kind {
            CliProviderKind::Local => ProviderKind::Local,
            CliProviderKind::OpenaiCompatible => ProviderKind::OpenaiCompatible,
            CliProviderKind::Anthropic => ProviderKind::Anthropic,
            CliProviderKind::Custom => ProviderKind::Custom,
        }
    }
}

/// CLI mirror of [`PrivacyMode`].
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPrivacyMode {
    /// Content never leaves the machine.
    LocalOnly,
    /// Content may go remote after redaction (MVP-optional).
    RedactThenRemote,
}

impl From<CliPrivacyMode> for PrivacyMode {
    fn from(mode: CliPrivacyMode) -> Self {
        match mode {
            CliPrivacyMode::LocalOnly => PrivacyMode::LocalOnly,
            CliPrivacyMode::RedactThenRemote => PrivacyMode::RedactThenRemote,
        }
    }
}

/// CLI mirror of [`Phase`].
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPhase {
    /// Requirement authoring.
    Spec,
    /// Architecture / task decomposition.
    Architect,
    /// Implementation from task artifacts.
    Implement,
    /// Code review.
    Review,
    /// Mechanical I/O.
    Io,
    /// No structured phase.
    Freeform,
}

impl From<CliPhase> for Phase {
    fn from(phase: CliPhase) -> Self {
        match phase {
            CliPhase::Spec => Phase::Spec,
            CliPhase::Architect => Phase::Architect,
            CliPhase::Implement => Phase::Implement,
            CliPhase::Review => Phase::Review,
            CliPhase::Io => Phase::Io,
            CliPhase::Freeform => Phase::Freeform,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = socket_path::daemon_paths();

    let result = match cli.command {
        None => run_session(&paths, cli.yes, cli.verbose),
        Some(Command::Doctor) => run_doctor(&paths),
        Some(Command::Cost) => run_cost(&paths),
        Some(Command::Model { action }) => match action {
            ModelAction::List => run_model_list(&paths),
            ModelAction::Set { name } => run_model_set(&paths, &name, cli.yes),
            ModelAction::Status => run_model_status(&paths),
        },
        Some(Command::Provider { action }) => match action {
            ProviderAction::Add {
                id,
                kind,
                endpoint,
                model,
            } => run_provider_add(&paths, &id, kind.into(), endpoint, model),
            ProviderAction::List => run_provider_list(&paths),
        },
        Some(Command::Boundary { action }) => match action {
            BoundaryAction::Add { glob, mode } => run_boundary_add(&paths, glob, mode.into()),
            BoundaryAction::List => run_boundary_list(&paths),
        },
        Some(Command::Policy { action }) => match action {
            PolicyAction::Set {
                phase,
                provider,
                fallback,
            } => run_policy_set(&paths, phase.into(), provider, fallback),
            PolicyAction::Show => run_policy_show(&paths),
        },
        Some(Command::Uninstall { keep_data }) => run_uninstall(&paths, keep_data, cli.yes),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("teton: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// How long the interactive entry loop waits on stdin before checking the
/// daemon's event stream and advancing the loading indicator (REQ-556).
///
/// Short enough that a lifecycle line lands promptly and an animation reads as
/// motion; long enough that an idle session is not spinning. This is the
/// indicator's frame interval as well as the poll timeout — one clock, so there
/// is no timer thread and no `sleep` anywhere in the loop.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// How many rows above the cursor the indicator's row sits while the entry
/// frame is open: `[status][top rule][input row ← cursor][bottom rule]`.
///
/// Lives beside the frame interval because both are facts about the interactive
/// layout that `next_interactive_line` depends on; the geometry itself is
/// `FramedStdinPrompter::draw`'s, and this must move if that does.
const STATUS_ROWS_ABOVE_CURSOR: usize = 2;

/// Wait for the next line an interactive user types, rendering anything the
/// daemon says in the meantime (REQ-556 BR-1).
///
/// The entry frame stays **open** across the wait, so the place to type is
/// visible the whole time. When something does arrive, the frame is torn down
/// before it renders and redrawn afterwards — otherwise the notice would land
/// in the input row and shred it. Nothing queued means no teardown at all, so
/// an idle session does not flicker once per interval.
///
/// `None` on EOF (Ctrl-D), which the caller turns into the same post-loop
/// session-end path `/quit` takes (REQ-555 BR-6).
///
/// # Errors
///
/// Propagates a failure from answering a permission or model proposal.
fn next_interactive_line(
    entry_prompt: &str,
    entry: &mut FramedStdinPrompter,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    tick: &mut u64,
) -> anyhow::Result<Option<String>> {
    // The indicator's row is emitted through the `Surface` seam (BR-3) and the
    // frame through the `Prompter` seam, in that order, so the indicator sits
    // immediately above the frame's top rule (ADR-556-4). `status_rows` is how
    // many rows that added, which is what `erase` needs to take back.
    let mut status_rows = paint_indicator(ctx, *tick);
    entry.draw(entry_prompt);
    loop {
        if prompt::stdin_ready(FRAME_INTERVAL) {
            // Readable covers "a line is waiting" and "the descriptor is at
            // EOF"; `read_line` distinguishes them by yielding zero bytes.
            return Ok(entry.read_line());
        }
        // Nothing typed within the interval. Anything from the daemon? The
        // teardown only runs if something is actually going to render, so an
        // idle session does not flicker once per interval.
        let rows = status_rows;
        let drained = conn.drain_events(ctx, || entry.erase(rows))?;
        if drained.rendered > 0 {
            status_rows = paint_indicator(ctx, *tick);
            entry.draw(entry_prompt);
            continue;
        }
        // Nothing typed and nothing said: advance the animation, if there is
        // one running. A hidden or stalled indicator asks for no repaint, so a
        // settled session costs one `poll` per interval and nothing else.
        //
        // This repaints the status row **in place** rather than tearing the
        // frame down. The frame teardown above is correct for an event, which
        // has to scroll a new line into the log; using it for the animation
        // would blank whatever the user had typed into the input row eight
        // times a second (ADR-556-4). The text would survive — the kernel holds
        // the line until Enter — but watching it flicker away while typing is
        // not a thing to ship.
        if ctx.state.loading.tick() {
            *tick = tick.wrapping_add(1);
            if let Some(line) = ctx.state.loading.frame(*tick) {
                // Two rows up: the layout is [status][top rule][input row ←
                // cursor][bottom rule].
                ctx.surface
                    .repaint_row_above(STATUS_ROWS_ABOVE_CURSOR, LineKind::Notice, &line);
                status_rows = 1;
            }
        }
    }
}

/// Draw the indicator's row, if it has anything to say. Returns how many rows
/// it occupies, which is `0` when nothing was drawn.
///
/// Goes through [`Surface::line`] rather than writing to stdout, so a ratatui
/// front-end inherits the indicator by implementing the same seam (BR-3).
fn paint_indicator(ctx: &mut UiContext<'_>, tick: u64) -> usize {
    match ctx.state.loading.frame(tick) {
        Some(line) => {
            ctx.surface.line(LineKind::Notice, &line);
            1
        }
        None => 0,
    }
}

/// The headline a turn that arrived while the local tier was still coming up
/// renders under (BUG-152).
///
/// The daemon's own sentence follows it and carries the detail — which model,
/// how far along it is, and what someone who does not want to wait can do
/// instead. This says only the part that decides whether to read the rest: no
/// action is required, and the tier is on its way.
const TIER_WARMING_HEADLINE: &str = "model still loading —";

/// Render a `prompt/turn` the daemon answered with an error.
///
/// Two classes, told apart by the daemon's own code rather than by re-reading
/// its sentence here (BUG-152). A tier that is still coming up — weights
/// downloading, or loaded and being benchmarked — is not a failure: nothing
/// broke, the user has nothing to fix, and the state ends by itself. It renders
/// as a [`LineKind::Notice`], the same class as the startup lifecycle lines it
/// is a continuation of. Everything else is a real failure and keeps the error
/// line and its `prompt failed:` prefix.
///
/// Split out of the entry loop so both arms are testable without a socket, for
/// the reason [`cost_report_or_report`] is: the arms *are* the behaviour, and a
/// branch only an e2e can reach is a branch that gets asserted on by accident.
fn render_turn_failure(err: &RpcError, surface: &mut dyn Surface) {
    if err.code == error_code::TIER_WARMING {
        surface.line(
            LineKind::Notice,
            &format!("{TIER_WARMING_HEADLINE} {}", err.message),
        );
    } else {
        surface.line(LineKind::Error, &format!("prompt failed: {}", err.message));
    }
}

/// The default experience: an interactive freeform session (AC-1).
///
/// This is the client that owns the first-run model prompt: it answers permission
/// requests and model proposals, and `auto_accept` (`--yes`) makes the latter
/// unattended (BR-5).
fn run_session(paths: &DaemonPaths, auto_accept: bool, verbose: bool) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut state = SessionState::new();
    state.verbose = verbose;
    let mut prompter = StdinPrompter::new();

    // The banner is for humans at a terminal. Piped stdout (the e2e suites,
    // shell composition) sees the same byte stream it always did.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // The *other* half of "interactive", read once here at the edge and carried
    // on the context (REQ-555): where the entry lines come from, which is what
    // the `/model set` gate turns on. Two different questions — a session may
    // well have a piped stdin and a terminal stdout — so neither flag is
    // derivable from the other, and a handler must never read either itself.
    let typed_input = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let color = interactive && banner::color_enabled();
    if interactive {
        banner::print(
            &mut surface,
            env!("CARGO_PKG_VERSION"),
            banner::cwd_display().as_deref(),
            color,
        );
    }

    // The session path may first offer to register the launchd service (the
    // install-side mirror of `teton uninstall`); every subcommand keeps the
    // plain autostart.
    let mut conn = client::ensure_connected_session(paths, &mut surface, &mut prompter)?;

    {
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: auto_accept,
            typed_input,
        };

        // A proposal raised before this client attached is never replayed as an
        // event (REQ-547 TASK-004), so look for one before doing anything else —
        // otherwise the local tier would stay gated with no visible reason.
        conn.answer_outstanding_model_proposal(&mut ctx)?;

        let created = conn.call(
            SessionCreateParams {
                mode: SessionMode::Freeform,
                phase: None,
                // BUG-147: the daemon runs under launchd (cwd `/`); the tool
                // jail must be THIS terminal's directory, so send it.
                cwd: std::env::current_dir().ok(),
            },
            &mut ctx,
        )?;
        let session_id = match created {
            Ok(res) => res.session_id,
            Err(err) => {
                ctx.surface.line(
                    LineKind::Error,
                    &format!("could not start a session: {}", err.message),
                );
                return Ok(());
            }
        };
        ctx.surface.line(
            LineKind::Info,
            &format!(
                "session {session_id} ready (freeform). Type a prompt or /help for commands; \
                 Ctrl-D to end."
            ),
        );
        if interactive {
            // A blank line so the entry area sits clear of the status text.
            ctx.surface.line(LineKind::Info, "");
        }

        // The entry area gets its own framed prompter; permission and model
        // questions keep the plain one in `ctx` — they are dialogue, not entry.
        let entry_prompt = if interactive {
            if color {
                " \x1b[36m›\x1b[0m "
            } else {
                " › "
            }
        } else {
            "› "
        };
        let mut entry = FramedStdinPrompter::new(interactive, color);
        // The indicator's animation clock, persisted across turns so the dots do
        // not restart every time a turn ends (REQ-556).
        let mut frame_tick: u64 = 0;
        loop {
            // REQ-556 BR-1. An interactive session waits on stdin *and* the
            // daemon's event stream, so a lifecycle line reaches the user when
            // it happens rather than at the next turn. A piped session takes
            // the original blocking path untouched — that is BR-2's
            // byte-identity holding by construction rather than by care, since
            // the new code is simply not on that path.
            let input = if interactive {
                match next_interactive_line(
                    entry_prompt,
                    &mut entry,
                    &mut conn,
                    &mut ctx,
                    &mut frame_tick,
                )? {
                    Some(line) => line,
                    None => break,
                }
            } else {
                match entry.ask(entry_prompt) {
                    Some(line) => line,
                    None => break,
                }
            };
            let text = input.trim();
            if text.is_empty() {
                continue;
            }
            // Slash commands are intercepted before any RPC is built (BR-1), so
            // a command never reaches the model, the transcript, or the meter.
            let prompt_text = match slash::classify(text) {
                slash::Input::Command { name, args } => {
                    match slash::dispatch(name, args, &mut conn, &mut ctx)? {
                        slash::CommandOutcome::Continue => continue,
                        // `/quit` leaves through the same post-loop path Ctrl-D
                        // takes — session-end cost summary, no `process::exit`
                        // and no parallel shutdown to drift from it (BR-6).
                        slash::CommandOutcome::Quit => break,
                    }
                }
                // The escape hatch has already collapsed its leading pair
                // (BR-1b); a plain prompt is the trimmed line's own bytes.
                slash::Input::EscapedPrompt(text) | slash::Input::Prompt(text) => text,
            };
            // Built by the classifier's own module, so the bytes on the wire are
            // the bytes it classified (AC-7 / AC-7b) rather than a second
            // reading of the line taken here.
            let params = slash::prompt_turn_params(&session_id, prompt_text);
            match conn.call(params, &mut ctx)? {
                Ok(res) => {
                    // Gated on session state, not on the `--verbose` flag, so
                    // `/verbose` governs this line and the routing notices from
                    // one source of truth (D-5); the flag only initialises it.
                    if ctx.state.verbose {
                        ctx.surface.line(
                            LineKind::Info,
                            &format!("turn ended ({:?}).", res.stop_reason),
                        );
                    } else {
                        // The streamed response may not end in a newline; a blank
                        // line closes it so the next entry frame starts clean.
                        ctx.surface.line(LineKind::Info, "");
                    }
                }
                Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
                    ctx.surface.line(
                        LineKind::Notice,
                        "this daemon build does not execute prompt turns yet (turn-loop wiring \
                         lands with TASK-013); session and event rendering are ready.",
                    );
                    break;
                }
                Err(err) => render_turn_failure(&err, ctx.surface),
            }
        }
    }

    // Session-end cost summary (AC-4). Ask the daemon's authoritative `cost/query`
    // RPC and render its report directly — the CLI recomputes no spend or savings
    // (REQ-544 M-7). The live meter supplies only the session call count.
    let session_line = if state.cost.is_empty() {
        "no model calls were recorded this session.".to_owned()
    } else {
        format!("recorded {} model call(s) this session.", state.cost.len())
    };
    surface.line(LineKind::Info, &session_line);
    {
        // The session is over by the time this runs, so the summary asks with a
        // passive context exactly as it always has.
        let mut end_ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
        query_and_render_cost(&mut conn, &mut end_ctx)?;
    }
    let _ = surface.flush();
    Ok(())
}

/// Query the daemon's authoritative cost report (`cost/query`, BR-2 / AC-4) and
/// render it, or print a graceful notice when the daemon does not expose the
/// method or cannot answer. Every figure — totals, baseline, savings — comes from
/// the daemon; the CLI computes none of it (REQ-544 M-7).
///
/// This is the **one** implementation behind every cost surface: `teton cost`,
/// the session-end summary, and the in-session `/cost` command (REQ-555 BR-4 /
/// AC-2) are call sites of it, never re-implementations — two surfaces
/// describing the same daemon state must not be able to drift apart. The caller
/// supplies the context, so the subcommand keeps its passive one while `/cost`
/// runs under the session's own (REQ-555 D-4).
pub(crate) fn query_and_render_cost(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    let answered = conn.call(CostQueryParams::default(), ctx)?;
    if let Some(report) = cost_report_or_report(answered, ctx.surface) {
        cost_ui::render_report_view(&report, ctx.surface);
    }
    Ok(())
}

/// Unwrap a `cost/query` answer, rendering the daemon's failure arms.
///
/// The counterpart of [`model_status_or_report`], and for the same reason: a
/// daemon too old to serve the method — or one that answers with an error —
/// must say the same thing on `teton cost`, on the session-end summary, and on
/// the in-session `/cost` (REQ-555 BR-4). All three run
/// [`query_and_render_cost`], so they already shared these strings; splitting
/// the arms out is what makes them *testable* without a socket, which is the
/// half that was missing.
///
/// `None` means the reason is already on the surface and the caller renders
/// nothing further.
fn cost_report_or_report(
    answered: Result<CostQueryResult, RpcError>,
    surface: &mut dyn Surface,
) -> Option<CostReportView> {
    match answered {
        Ok(res) => Some(res.report),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            surface.line(
                LineKind::Notice,
                "this daemon build does not expose the cost/query method yet; no authoritative \
                 cost report is available.",
            );
            None
        }
        Err(err) => {
            surface.line(
                LineKind::Error,
                &format!("cost query failed: {}", err.message),
            );
            None
        }
    }
}

/// A context for a one-shot command: it renders the daemon's broadcasts but
/// answers nothing. Permission requests and model proposals belong to whichever
/// interactive session owns them — a `teton cost` running in another terminal
/// must not silently answer a prompt the user is looking at elsewhere.
///
/// `typed_input` is read here too, though no subcommand consults it: the field
/// belongs to the slash handlers, and no slash command runs under a passive
/// context. Filling it with the same edge check the session makes keeps it an
/// honest description of the process rather than a placeholder that would
/// quietly become a gate answer if a future caller did read it.
fn passive_ctx<'a>(
    surface: &'a mut dyn Surface,
    state: &'a mut SessionState,
    prompter: &'a mut dyn Prompter,
) -> UiContext<'a> {
    UiContext {
        surface,
        state,
        prompter,
        answer_permissions: false,
        answer_model_proposals: false,
        auto_accept_model: false,
        typed_input: std::io::IsTerminal::is_terminal(&std::io::stdin()),
    }
}

/// `teton model list`: the catalog, each entry's fit, and the selection (AC-9).
fn run_model_list(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    match conn.call(ModelListParams::default(), &mut ctx)? {
        Ok(list) => model_ui::render_list(&list, ctx.surface),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not expose model/list yet.",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read the model catalog: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton model set <name>`: change the selection post-first-run (AC-9).
///
/// A connection-opening shell around [`apply_model_set`] and nothing else: the
/// validation, the BR-3 confirmation, and the `model/set` call are the shared
/// flow's, so this subcommand and the in-session `/model set` cannot diverge
/// (REQ-555 BR-4b).
fn run_model_set(paths: &DaemonPaths, name: &str, assume_yes: bool) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    apply_model_set(name, assume_yes, &mut conn, &mut ctx)
}

/// Change the local-tier model selection: validate the name against
/// `model/list`, apply the REQ-547 BR-3 above-RAM-floor confirmation, and send
/// `model/set`.
///
/// This is the **one** implementation of that flow (REQ-555 BR-4b, D-3). Both
/// `teton model set` and the in-session `/model set` are call sites of it — the
/// caller supplies an already-open connection and its own context, so the
/// subcommand keeps its passive ctx while `/model set` runs under the session's
/// (D-4). A parallel copy of a confirmation flow is exactly how REQ-547's
/// consent bypass shipped (LESSON-441): the branch that skipped the check was
/// the one nobody was looking at.
///
/// The BR-3 second confirmation exists for the same reason it exists in the
/// first-run prompt: an above-RAM-floor pick is the user's call but must never
/// happen by accident. The fit comes from `model/list` (the daemon computes it),
/// and the daemon independently refuses the change unless
/// `confirmed_above_ram_floor` is set — this is the legible half of that guard,
/// not the guard itself.
///
/// # Errors
///
/// Propagates a transport error from either RPC. A daemon that *answers* — with
/// an error, or with "no such method" — is reported on the surface and returns
/// `Ok`: a refused change ends the command, never the session.
pub(crate) fn apply_model_set(
    name: &str,
    assume_yes: bool,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    let list = match conn.call(ModelListParams::default(), ctx)? {
        Ok(list) => list,
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            ctx.surface.line(
                LineKind::Notice,
                "this daemon build does not expose model/list yet, so the choice cannot be \
                 checked against this machine.",
            );
            return Ok(());
        }
        Err(err) => {
            ctx.surface.line(
                LineKind::Error,
                &format!("could not read the model catalog: {}", err.message),
            );
            return Ok(());
        }
    };

    // Everything between the two RPCs is the pure decision below, so the
    // consent gate is exercised by unit tests with no daemon and no socket.
    let Some(params) = decide_model_set(
        name,
        assume_yes,
        &list,
        &mut *ctx.surface,
        &mut *ctx.prompter,
    ) else {
        return Ok(());
    };

    match conn.call(params, ctx)? {
        Ok(result) => {
            let source = firstrun::source_label(result.selection.source);
            ctx.surface.line(
                LineKind::Info,
                &format!(
                    "selection: {} ({source}) — the daemon installs the weights if they are \
                     missing.",
                    result.selection.model_name.as_deref().unwrap_or(name)
                ),
            );
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not expose model/set yet.",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("the daemon refused the change: {}", err.message),
        ),
    }
    Ok(())
}

/// Decide what — if anything — `model/set` should be asked to do about `name`.
///
/// Pure with respect to the daemon: it reads one `model/list` payload and, for
/// an above-floor pick, asks one question. `None` means send nothing; the reason
/// is already on the surface. Splitting this out of [`apply_model_set`] is what
/// lets the BR-3 gate — including the leg where the user declines — be pinned
/// without a `Connection` (LESSON-441/464: a consent gate needs its own
/// known-bad).
fn decide_model_set(
    name: &str,
    assume_yes: bool,
    list: &ModelListResult,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> Option<ModelSetParams> {
    let Some(model) = list.models.iter().find(|m| m.entry.name == name) else {
        let names: Vec<&str> = list.models.iter().map(|m| m.entry.name.as_str()).collect();
        surface.line(
            LineKind::Error,
            &format!(
                "no catalog entry named `{name}`. Available: {}",
                names.join(", ")
            ),
        );
        return None;
    };

    // BR-3: above this machine's RAM floor needs an explicit second answer.
    let above_floor = model.entry.ram_floor_bytes > list.probe.total_ram_bytes;
    if above_floor && !assume_yes {
        let confirmed = model_ui::confirm_above_ram_floor(
            name,
            model.entry.ram_floor_bytes,
            list.probe.total_ram_bytes,
            surface,
            prompter,
        );
        if !confirmed {
            surface.line(
                LineKind::Notice,
                &format!("selection unchanged; `{name}` was not sent."),
            );
            return None;
        }
    } else if above_floor {
        surface.line(
            LineKind::Notice,
            &format!(
                "`{name}` needs more RAM than this machine has; --yes supplies the second \
                 confirmation (BR-3)."
            ),
        );
    }

    Some(ModelSetParams {
        name: name.to_owned(),
        confirmed_above_ram_floor: above_floor,
    })
}

/// `teton model status`: the decision, the install state, and where the weights
/// live (AC-9).
///
/// The path is derived here from the daemon state directory rather than received:
/// `InstallStateView` carries no path, because BR-11 keeps absolute filesystem
/// paths out of every protocol payload. Showing it locally is explicitly allowed.
fn run_model_status(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let answered = conn.call(ModelStatusParams::default(), &mut ctx)?;
    if let Some(status) = model_status_or_report(answered, ctx.surface) {
        let base_dir = paths.socket.parent();
        let path = match (base_dir, status.install.as_ref()) {
            (Some(base), Some(install)) => Some(model_ui::weights_path(base, &install.model_name)),
            _ => None,
        };
        model_ui::render_status(&status, path.as_deref(), ctx.surface);
    }
    Ok(())
}

/// Unwrap a `model/status` answer, rendering the daemon's failure arms.
///
/// The **one** place those two arms are worded (REQ-555 BR-4): `teton model
/// status` and the in-session `/model` are both call sites, so a daemon too old
/// to serve the method — or one that answers with an error — says the same thing
/// on both surfaces. Only the success rendering differs between them, which is
/// exactly what D-6 says differs.
///
/// `None` means the reason is already on the surface and the caller renders
/// nothing further. Taking the answered `Result` rather than the connection is
/// what lets both arms be unit-tested with no socket.
fn model_status_or_report(
    answered: Result<ModelStatusResult, RpcError>,
    surface: &mut dyn Surface,
) -> Option<ModelStatusResult> {
    match answered {
        Ok(status) => Some(status),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            surface.line(
                LineKind::Notice,
                "this daemon build does not expose model/status yet.",
            );
            None
        }
        Err(err) => {
            surface.line(
                LineKind::Error,
                &format!("could not read the model status: {}", err.message),
            );
            None
        }
    }
}

/// `teton doctor`: daemon status, socket path, model state, providers.
fn run_doctor(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    surface.line(LineKind::Info, "teton doctor");
    surface.line(
        LineKind::Info,
        &format!("socket: {}", paths.socket.display()),
    );
    surface.line(LineKind::Info, &format!("lock:   {}", paths.lock.display()));

    match Connection::connect(&paths.socket) {
        Ok(mut conn) => match conn.handshake() {
            Ok(hs) => {
                surface.line(
                    LineKind::Info,
                    &format!(
                        "daemon: running — {} {} (protocol {})",
                        hs.daemon_name, hs.daemon_version, hs.protocol_version
                    ),
                );
                let mut state = SessionState::new();
                let mut prompter = StdinPrompter::new();
                let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
                match conn.call(ConfigGetParams::default(), &mut ctx)? {
                    Ok(cfg) => render_config(&cfg.snapshot.providers, ctx.surface),
                    Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
                        LineKind::Notice,
                        "config: not exposed by this daemon build yet (config/get pending).",
                    ),
                    Err(err) => ctx.surface.line(
                        LineKind::Error,
                        &format!("config query failed: {}", err.message),
                    ),
                }
            }
            Err(err) => surface.line(
                LineKind::Error,
                &format!("daemon: reachable but handshake failed: {err}"),
            ),
        },
        Err(_) => surface.line(
            LineKind::Notice,
            "daemon: not running (run `teton` to autostart it, or start `teton-code`).",
        ),
    }

    surface.line(
        LineKind::Notice,
        "model: the local-tier lifecycle is event-driven — start a session to observe \
         probe/download/benchmark.",
    );
    surface.line(
        LineKind::Notice,
        "providers: reachability is probed by the daemon at call time; the CLI has no network \
         path of its own (BR-1).",
    );
    Ok(())
}

/// `teton uninstall`: show the removal plan, confirm, run it (see the
/// `uninstall` module docs for why this exists — Homebrew formulae have no
/// uninstall hook, so `brew uninstall teton` alone strands the daemon, the
/// model, and the logs).
fn run_uninstall(paths: &DaemonPaths, keep_data: bool, auto_accept: bool) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut prompter = StdinPrompter::new();
    let brew_prefix = uninstall::brew_prefix();
    let tap_registered = brew_prefix.is_some() && uninstall::tap_registered();
    let plan = uninstall::Plan::build(paths, keep_data, brew_prefix, tap_registered);
    uninstall::run(paths, &plan, auto_accept, &mut surface, &mut prompter)
}

/// `teton cost`: render the daemon's authoritative persisted cost report (AC-4,
/// BR-2). Sources every figure from the daemon's `cost/query` RPC — no live-event
/// draining, no client-side repricing (REQ-544 M-7).
fn run_cost(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    {
        let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
        query_and_render_cost(&mut conn, &mut ctx)?;
    }
    let _ = surface.flush();
    Ok(())
}

/// `teton provider add`: store the key in the keychain (BR-7), then register.
fn run_provider_add(
    paths: &DaemonPaths,
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    // REQ-557 BR-1 / TASK-046: a remote provider MUST declare its model, and the
    // check runs BEFORE `read_secret` — otherwise the user types a credential
    // into a command that was always going to fail.
    if !matches!(kind, ProviderKind::Local) && model.as_deref().unwrap_or("").trim().is_empty() {
        anyhow::bail!(
            "provider `{id}` is a remote provider and must declare the model it calls: \
             pass `--model <name>` (e.g. `--model claude-opus-5`). The model is never \
             inferred from the provider id."
        );
    }
    let keychain = keychain::default_keychain();
    // Local providers have no credential; every remote kind requires a key.
    let secret = if matches!(kind, ProviderKind::Local) {
        None
    } else {
        Some(read_secret(id)?)
    };
    let config = build_provider_registration(
        id,
        kind,
        endpoint,
        model,
        keychain.as_ref(),
        secret.as_deref(),
    )?;
    let auth = config.auth_ref.clone().unwrap_or_else(|| "—".to_owned());

    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);

    let params = ConfigSetParams {
        update: ConfigUpdate::RegisterProvider(config),
    };
    match conn.call(params, &mut ctx)? {
        Ok(res) if res.applied => ctx.surface.line(
            LineKind::Info,
            &format!(
                "provider `{id}` registered ({}). Key stored in the OS keychain (ref {auth}); \
                 no key written to disk.",
                kind_label(kind)
            ),
        ),
        Ok(_) => ctx.surface.line(
            LineKind::Notice,
            &format!("provider `{id}`: the daemon did not apply the registration."),
        ),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            &format!(
                "provider `{id}`: key stored in the OS keychain (ref {auth}); this daemon build \
                 does not implement config/set yet, so registration is pending TASK-013."
            ),
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("provider `{id}` registration rejected: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton provider list`.
fn run_provider_list(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    match conn.call(ConfigGetParams::default(), &mut ctx)? {
        Ok(cfg) => render_config(&cfg.snapshot.providers, ctx.surface),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/get yet (wiring in progress).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read config: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton boundary add`.
fn run_boundary_add(paths: &DaemonPaths, glob: String, mode: PrivacyMode) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let params = ConfigSetParams {
        update: ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
            path_glob: glob.clone(),
            mode,
        }),
    };
    match conn.call(params, &mut ctx)? {
        Ok(res) if res.applied => ctx.surface.line(
            LineKind::Info,
            &format!("boundary added: {glob} [{}]", privacy_label(mode)),
        ),
        Ok(_) => ctx
            .surface
            .line(LineKind::Notice, "the daemon did not apply the boundary."),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/set yet (wiring in progress).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("boundary rejected: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton boundary list`.
fn run_boundary_list(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    match conn.call(ConfigGetParams::default(), &mut ctx)? {
        Ok(cfg) => {
            if cfg.snapshot.privacy.is_empty() {
                ctx.surface.line(
                    LineKind::Info,
                    "no privacy boundaries configured. Add one with `teton boundary add`.",
                );
            } else {
                ctx.surface.line(LineKind::Info, "privacy boundaries:");
                for boundary in &cfg.snapshot.privacy {
                    ctx.surface.line(
                        LineKind::Info,
                        &format!(
                            "  {} [{}]",
                            boundary.path_glob,
                            privacy_label(boundary.mode)
                        ),
                    );
                }
            }
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/get yet (wiring in progress).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read config: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton policy set`.
fn run_policy_set(
    paths: &DaemonPaths,
    phase: Phase,
    provider: String,
    fallback: Option<String>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let params = ConfigSetParams {
        update: ConfigUpdate::SetRoutingRule(RoutingRule {
            phase,
            provider_id: ProviderId::from(provider.as_str()),
            fallback_id: fallback.as_deref().map(ProviderId::from),
        }),
    };
    match conn.call(params, &mut ctx)? {
        Ok(res) if res.applied => ctx.surface.line(
            LineKind::Info,
            &format!("policy set: {phase:?} → {provider}"),
        ),
        Ok(_) => ctx.surface.line(
            LineKind::Notice,
            "the daemon did not apply the routing rule.",
        ),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/set yet (wiring in progress).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("routing rule rejected: {}", err.message),
        ),
    }
    Ok(())
}

/// `teton policy show`.
fn run_policy_show(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    match conn.call(ConfigGetParams::default(), &mut ctx)? {
        Ok(cfg) => {
            if cfg.snapshot.routing.is_empty() {
                ctx.surface.line(
                    LineKind::Info,
                    "no routing rules configured. Set one with `teton policy set`.",
                );
            } else {
                ctx.surface.line(LineKind::Info, "routing policy:");
                for rule in &cfg.snapshot.routing {
                    let fallback = rule
                        .fallback_id
                        .as_ref()
                        .map_or_else(String::new, |f| format!(" (fallback {f})"));
                    ctx.surface.line(
                        LineKind::Info,
                        &format!("  {:?} → {}{fallback}", rule.phase, rule.provider_id),
                    );
                }
            }
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/get yet (wiring in progress).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read config: {}", err.message),
        ),
    }
    Ok(())
}

/// Build the provider registration, storing any secret in the keychain first so
/// only the reference travels onward (BR-7).
fn build_provider_registration(
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    model: Option<String>,
    keychain: &dyn Keychain,
    secret: Option<&str>,
) -> anyhow::Result<ProviderConfig> {
    let auth_ref = match secret {
        Some(secret) => Some(keychain.store(id, secret)?),
        None => None,
    };
    Ok(ProviderConfig {
        id: ProviderId::from(id),
        kind,
        endpoint,
        model,
        auth_ref,
    })
}

/// Read a provider API key from `TETON_PROVIDER_KEY` or, failing that, stdin.
/// The key is handed straight to the keychain and never written to a file.
fn read_secret(id: &str) -> anyhow::Result<String> {
    if let Ok(key) = std::env::var("TETON_PROVIDER_KEY") {
        let key = key.trim().to_owned();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let mut prompter = StdinPrompter::new();
    match prompter.ask(&format!(
        "API key for `{id}` (read from stdin, stored only in the keychain): "
    )) {
        Some(key) if !key.trim().is_empty() => Ok(key.trim().to_owned()),
        _ => anyhow::bail!("no API key provided; set TETON_PROVIDER_KEY or enter the key"),
    }
}

/// Render a provider list to a surface.
fn render_config(providers: &[ProviderConfig], surface: &mut dyn Surface) {
    if providers.is_empty() {
        surface.line(
            LineKind::Info,
            "no providers configured. Add one with `teton provider add`.",
        );
        return;
    }
    surface.line(LineKind::Info, "providers:");
    for provider in providers {
        let endpoint = provider.endpoint.as_deref().unwrap_or("(local)");
        let auth = if provider.auth_ref.is_some() {
            "keychain"
        } else {
            "none"
        };
        // REQ-557 BR-1/BR-3: the model a provider calls is what distinguishes two
        // otherwise-identical providers ("Opus for design, Sonnet for build"), so
        // it is the field this listing exists to show. A remote provider with no
        // model cannot serve turns (ADR-E) — say so here rather than printing a
        // blank column, because this listing is where a user goes to find out why
        // a provider stopped working after an upgrade.
        let model = match (provider.model.as_deref(), provider.kind) {
            (Some(model), _) if !model.trim().is_empty() => model.to_owned(),
            // The local tier's model is owned by the REQ-547 consent flow, not by
            // this field; `teton model status` is where it is read (OQ-4).
            (_, ProviderKind::Local) => "(see `teton model status`)".to_owned(),
            _ => "UNUSABLE — no model; re-add with `--model <name>`".to_owned(),
        };
        surface.line(
            LineKind::Info,
            &format!(
                "  {} [{}]  {model}  {endpoint}  auth: {auth}",
                provider.id,
                kind_label(provider.kind)
            ),
        );
    }
}

/// Wire-name label for a provider kind.
fn kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local => "local",
        ProviderKind::OpenaiCompatible => "openai-compatible",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Custom => "custom",
    }
}

/// Wire-name label for a privacy mode.
fn privacy_label(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::LocalOnly => "local-only",
        PrivacyMode::RedactThenRemote => "redact-then-remote",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MockKeychain;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;

    /// Parse args as the CLI would, panicking with clap's message on error.
    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn no_subcommand_opens_a_session() {
        let cli = parse(&["teton"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn doctor_and_cost_parse() {
        assert!(matches!(
            parse(&["teton", "doctor"]).command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            parse(&["teton", "cost"]).command,
            Some(Command::Cost)
        ));
    }

    #[test]
    fn auto_accept_is_off_by_default_and_settable_on_either_side_of_a_subcommand() {
        // BR-5: prompting is the default for an interactive client; the
        // unattended path is explicit opt-in.
        assert!(!parse(&["teton"]).yes);
        assert!(parse(&["teton", "--yes"]).yes);
        assert!(parse(&["teton", "-y"]).yes);
        // `global = true` so it works after a subcommand too (`teton model set
        // <name> --yes` is where BR-3's second confirmation is supplied).
        assert!(parse(&["teton", "model", "set", "qwen2.5-coder-7b", "--yes"]).yes);
        assert!(!parse(&["teton", "model", "list"]).yes);
    }

    #[test]
    fn model_subcommands_parse() {
        assert!(matches!(
            parse(&["teton", "model", "list"]).command,
            Some(Command::Model {
                action: ModelAction::List
            })
        ));
        assert!(matches!(
            parse(&["teton", "model", "status"]).command,
            Some(Command::Model {
                action: ModelAction::Status
            })
        ));
        match parse(&["teton", "model", "set", "qwen2.5-coder-3b"]).command {
            Some(Command::Model {
                action: ModelAction::Set { name },
            }) => assert_eq!(name, "qwen2.5-coder-3b"),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn model_set_requires_a_name_and_rejects_unknown_actions() {
        assert!(Cli::try_parse_from(["teton", "model", "set"]).is_err());
        assert!(Cli::try_parse_from(["teton", "model", "nonsense"]).is_err());
    }

    #[test]
    fn provider_add_parses_kind_endpoint_and_model() {
        let cli = parse(&[
            "teton",
            "provider",
            "add",
            "deepseek",
            "--kind",
            "openai-compatible",
            "--endpoint",
            "https://api.deepseek.com",
            "--model",
            "deepseek-chat",
        ]);
        match cli.command {
            Some(Command::Provider {
                action:
                    ProviderAction::Add {
                        id,
                        kind,
                        endpoint,
                        model,
                    },
            }) => {
                assert_eq!(id, "deepseek");
                assert!(matches!(kind, CliProviderKind::OpenaiCompatible));
                assert_eq!(endpoint.as_deref(), Some("https://api.deepseek.com"));
                assert_eq!(ProviderKind::from(kind), ProviderKind::OpenaiCompatible);
                assert_eq!(model.as_deref(), Some("deepseek-chat"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    /// REQ-557 BR-3: two providers may share a kind and differ only in id and
    /// model — the "Opus for design, Sonnet for build" shape the REQ exists to
    /// make expressible. The parser must carry both models through distinctly.
    #[test]
    fn two_providers_of_one_kind_parse_to_distinct_models() {
        let model_of = |id: &str, model: &str| {
            let cli = parse(&[
                "teton",
                "provider",
                "add",
                id,
                "--kind",
                "anthropic",
                "--model",
                model,
            ]);
            match cli.command {
                Some(Command::Provider {
                    action:
                        ProviderAction::Add {
                            id, kind, model, ..
                        },
                }) => {
                    assert_eq!(ProviderKind::from(kind), ProviderKind::Anthropic);
                    (id, model)
                }
                other => panic!("unexpected parse: {other:?}"),
            }
        };
        let opus = model_of("opus", "claude-opus-5");
        let sonnet = model_of("sonnet", "claude-sonnet-5");
        assert_eq!(opus, ("opus".to_owned(), Some("claude-opus-5".to_owned())));
        assert_eq!(
            sonnet,
            ("sonnet".to_owned(), Some("claude-sonnet-5".to_owned()))
        );
    }

    /// REQ-557: `provider list` shows the model each provider calls — the field
    /// that distinguishes two providers of the same kind (BR-3) — and says
    /// plainly when a remote provider has none.
    ///
    /// The unusable branch matters most: after upgrading across REQ-557 a
    /// provider that was never migrated simply stops serving turns, and this
    /// listing is the first place a user looks. A blank column there would leave
    /// them to guess.
    #[test]
    fn provider_list_names_the_model_or_says_the_provider_is_unusable() {
        use teton_protocol::methods::ProviderConfig;

        let provider = |id: &str, kind: ProviderKind, model: Option<&str>| ProviderConfig {
            id: ProviderId::from(id),
            kind,
            endpoint: Some("https://example.invalid".to_owned()),
            model: model.map(str::to_owned),
            auth_ref: None,
        };

        let mut surface = RecordingSurface::new();
        render_config(
            &[
                provider("opus", ProviderKind::Anthropic, Some("claude-opus-5")),
                provider("sonnet", ProviderKind::Anthropic, Some("claude-sonnet-5")),
                provider("stale", ProviderKind::OpenaiCompatible, None),
                provider("on-device", ProviderKind::Local, None),
            ],
            &mut surface,
        );
        let rendered = surface.lines_of(LineKind::Info).join("\n");

        // Two providers, same kind, distinct models — the shape BR-3 exists for.
        assert!(rendered.contains("claude-opus-5"), "{rendered}");
        assert!(rendered.contains("claude-sonnet-5"), "{rendered}");
        // A remote provider with no model is called out, with the remedy.
        assert!(rendered.contains("UNUSABLE"), "{rendered}");
        assert!(rendered.contains("--model"), "{rendered}");
        // The local tier's model is owned by the consent flow, not this field —
        // so it is pointed at, never reported as broken (OQ-4).
        assert!(rendered.contains("teton model status"), "{rendered}");
        assert!(
            !rendered.contains("on-device [local]  UNUSABLE"),
            "a local provider without a model is normal, not unusable: {rendered}"
        );
    }

    /// `--model` is optional *to the parser* (a local provider legitimately has
    /// none — REQ-547 owns that selection) and required for a remote kind by
    /// `run_provider_add`, which rejects it before reading any credential. The
    /// split is deliberate: a parser-level `required` would break `provider add
    /// <id> --kind local`.
    #[test]
    fn provider_add_leaves_a_missing_model_to_the_runtime_check() {
        let cli = parse(&["teton", "provider", "add", "x", "--kind", "anthropic"]);
        match cli.command {
            Some(Command::Provider {
                action: ProviderAction::Add { model, .. },
            }) => assert_eq!(model, None),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn provider_add_requires_a_kind() {
        // `--kind` is mandatory; omitting it is a parse error.
        assert!(Cli::try_parse_from(["teton", "provider", "add", "x"]).is_err());
        // An unknown kind is rejected by the value-enum.
        assert!(
            Cli::try_parse_from(["teton", "provider", "add", "x", "--kind", "nonsense"]).is_err()
        );
    }

    #[test]
    fn boundary_add_defaults_to_local_only() {
        let cli = parse(&["teton", "boundary", "add", "secrets/**"]);
        match cli.command {
            Some(Command::Boundary {
                action: BoundaryAction::Add { glob, mode },
            }) => {
                assert_eq!(glob, "secrets/**");
                assert!(matches!(mode, CliPrivacyMode::LocalOnly));
                assert_eq!(PrivacyMode::from(mode), PrivacyMode::LocalOnly);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn policy_set_parses_phase_provider_and_fallback() {
        let cli = parse(&[
            "teton",
            "policy",
            "set",
            "implement",
            "deepseek",
            "--fallback",
            "anthropic",
        ]);
        match cli.command {
            Some(Command::Policy {
                action:
                    PolicyAction::Set {
                        phase,
                        provider,
                        fallback,
                    },
            }) => {
                assert!(matches!(phase, CliPhase::Implement));
                assert_eq!(Phase::from(phase), Phase::Implement);
                assert_eq!(provider, "deepseek");
                assert_eq!(fallback.as_deref(), Some("anthropic"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn provider_registration_stores_key_in_keychain_and_keeps_only_a_ref() {
        let keychain = MockKeychain::new();
        let config = build_provider_registration(
            "anthropic",
            ProviderKind::Anthropic,
            Some("https://api.anthropic.com".to_owned()),
            Some("claude-opus-5".to_owned()),
            &keychain,
            Some("sk-super-secret"),
        )
        .unwrap();

        // The config carries a reference, never the secret (BR-7).
        assert_eq!(
            config.auth_ref.as_deref(),
            Some("keychain://teton/anthropic")
        );
        // The secret went to the keychain, not into the config.
        assert_eq!(
            keychain.stored_secret("anthropic").as_deref(),
            Some("sk-super-secret")
        );
    }

    #[test]
    fn local_provider_registration_needs_no_secret() {
        let keychain = MockKeychain::new();
        let config =
            build_provider_registration("local", ProviderKind::Local, None, None, &keychain, None)
                .unwrap();
        assert!(config.auth_ref.is_none());
        assert!(keychain.stored_secret("local").is_none());
    }

    #[test]
    fn labels_match_wire_names() {
        assert_eq!(
            kind_label(ProviderKind::OpenaiCompatible),
            "openai-compatible"
        );
        assert_eq!(privacy_label(PrivacyMode::LocalOnly), "local-only");
    }

    // REQ-555 BR-4: `teton model status` and the in-session `/model` share these
    // two failure arms, so a daemon too old for the method — or one that answers
    // with an error — says the same thing on both surfaces. Testable at all
    // because the helper takes the *answered* result rather than the connection;
    // the success arm needs no test here, since each caller renders its own.
    #[test]
    fn a_failed_model_status_is_reported_the_same_way_for_both_surfaces() {
        let mut surface = RecordingSurface::new();
        let too_old = model_status_or_report(
            Err(RpcError::new(
                error_code::METHOD_NOT_FOUND,
                "no such method",
            )),
            &mut surface,
        );
        assert!(too_old.is_none(), "nothing to render from a refused method");
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["this daemon build does not expose model/status yet."]
        );

        let mut surface = RecordingSurface::new();
        let refused = model_status_or_report(
            Err(RpcError::new(
                error_code::INTERNAL_ERROR,
                "store unreadable",
            )),
            &mut surface,
        );
        assert!(refused.is_none());
        assert_eq!(
            surface.lines_of(LineKind::Error),
            vec!["could not read the model status: store unreadable"],
            "the daemon's own reason must survive to the user (LESSON-456)"
        );

        // And an answer passes straight through, so each surface renders its own
        // shape from the same payload.
        let mut surface = RecordingSurface::new();
        let status = ModelStatusResult {
            selection: None,
            install: None,
            pending_proposal: None,
        };
        assert!(model_status_or_report(Ok(status), &mut surface).is_some());
        assert!(
            surface.calls.is_empty(),
            "a successful status renders nothing of its own here: {:?}",
            surface.calls
        );
    }

    // REQ-556 BR-3 / AC-1 (unit leg). The indicator's row goes through the
    // `Surface` seam like every other line the session prints, so a ratatui
    // front-end inherits it by implementing the same seam rather than by
    // reimplementing the animation. The returned row count is what the frame's
    // `erase` takes back — get it wrong and the redraw strands a stale row.
    #[test]
    fn the_indicator_paints_through_the_surface_and_reports_its_row_count() {
        use teton_protocol::events::ModelLifecycleStage;

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: false,
                typed_input: true,
            };

            // Nothing observed yet: nothing drawn, no rows to take back.
            assert_eq!(paint_indicator(&mut ctx, 0), 0);

            // Mid-load: one row, through the seam, as a Notice like the
            // lifecycle lines it sits above.
            ctx.state.loading.observe(
                "qwen3-coder-30b-a3b",
                &ModelLifecycleStage::Benchmark {
                    first_token_ms: 368,
                    tokens_per_sec: 73.0,
                },
            );
            assert_eq!(paint_indicator(&mut ctx, 0), 1);

            // Tier open: back to nothing, so `erase` is told to take back zero
            // rows and the next frame sits flush where the ready line left off
            // (BR-6).
            ctx.state
                .loading
                .observe("qwen3-coder-30b-a3b", &ModelLifecycleStage::Ready);
            assert_eq!(paint_indicator(&mut ctx, 0), 0);
        }

        // Exactly one row across three paints — so the two hidden paints drew
        // nothing at all, which is what keeps an idle session quiet.
        assert_eq!(
            surface.lines_of(LineKind::Notice).len(),
            1,
            "exactly one indicator row was drawn across the three paints: {:?}",
            surface.calls
        );
        assert!(
            surface.any_line_contains(LineKind::Notice, "model starting"),
            "the drawn row is the loading motion: {:?}",
            surface.calls
        );
    }

    // BUG-152: the first prompt of a session often lands while the local tier
    // is still loading its weights, and reporting that as `error: prompt
    // failed:` told the user something had broken when nothing had. The split
    // is on the daemon's code — the client never re-reads the sentence for
    // keywords, which would be a second classifier for one state (LESSON-456).
    #[test]
    fn a_warming_tier_is_a_notice_and_every_other_refusal_is_still_an_error() {
        let mut surface = RecordingSurface::new();
        render_turn_failure(
            &RpcError::new(
                error_code::TIER_WARMING,
                "qwen3-coder-30b-a3b's weights are installed and verified; the daemon is \
                 loading and benchmarking them now. Retry in a moment.",
            ),
            &mut surface,
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a tier on its way up is not a failure: {:?}",
            surface.calls
        );
        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(notices.len(), 1, "exactly one line: {notices:?}");
        assert!(
            notices[0].starts_with(TIER_WARMING_HEADLINE),
            "the waiting headline leads, so it is what scans: {}",
            notices[0]
        );
        // The daemon's own sentence survives whole (LESSON-456): the headline
        // is added in front of the reason, never in place of it.
        assert!(
            notices[0].contains("loading and benchmarking them now")
                && notices[0].contains("Retry in a moment."),
            "the daemon's reason must reach the user intact: {}",
            notices[0]
        );

        // The other side of the split: everything that is genuinely broken —
        // or that needs the user to do something — still reads as an error.
        for code in [
            error_code::UNKNOWN_PROVIDER,
            error_code::INTERNAL_ERROR,
            error_code::PRIVACY_BLOCKED,
        ] {
            let mut surface = RecordingSurface::new();
            render_turn_failure(&RpcError::new(code, "the tier was declined"), &mut surface);
            assert_eq!(
                surface.lines_of(LineKind::Error),
                vec!["prompt failed: the tier was declined"],
                "code {code} must keep the failure line"
            );
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "code {code} is not a waiting state: {:?}",
                surface.calls
            );
        }
    }

    // The same shape for the cost surfaces (REQ-555 BR-4): `teton cost`, the
    // session-end summary and the in-session `/cost` are three call sites of
    // `query_and_render_cost`, so a daemon too old for `cost/query` — or one
    // that answers with an error — must say one thing on all three. The arms
    // were already shared; taking the *answered* result is what lets them be
    // asserted without a socket, which is the half a live-daemon e2e cannot
    // reach on purpose.
    #[test]
    fn a_failed_cost_query_is_reported_the_same_way_for_every_cost_surface() {
        let mut surface = RecordingSurface::new();
        let too_old = cost_report_or_report(
            Err(RpcError::new(
                error_code::METHOD_NOT_FOUND,
                "no such method",
            )),
            &mut surface,
        );
        assert!(too_old.is_none(), "nothing to render from a refused method");
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![
                "this daemon build does not expose the cost/query method yet; no authoritative \
                 cost report is available."
            ]
        );

        let mut surface = RecordingSurface::new();
        let refused = cost_report_or_report(
            Err(RpcError::new(error_code::INTERNAL_ERROR, "ledger locked")),
            &mut surface,
        );
        assert!(refused.is_none());
        assert_eq!(
            surface.lines_of(LineKind::Error),
            vec!["cost query failed: ledger locked"],
            "the daemon's own reason must survive to the user (LESSON-456)"
        );

        // And a report passes straight through untouched — every figure is the
        // daemon's, so this helper must not reshape one (REQ-544 M-7).
        let mut surface = RecordingSurface::new();
        let report = CostReportView {
            total_calls: 3,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            ..CostReportView::default()
        };
        assert_eq!(
            cost_report_or_report(
                Ok(CostQueryResult {
                    report: report.clone()
                }),
                &mut surface
            ),
            Some(report)
        );
        assert!(
            surface.calls.is_empty(),
            "a successful query renders nothing of its own here: {:?}",
            surface.calls
        );
    }

    // -----------------------------------------------------------------------
    // The shared `model set` flow (REQ-555 BR-4b / AC-3b). Both `teton model
    // set` and `/model set` run `apply_model_set`, whose whole decision — name
    // validation, the REQ-547 BR-3 above-RAM-floor gate, and what ends up in
    // `ModelSetParams` — is `decide_model_set`. Pinning it here pins both
    // surfaces at once; that is the point of there being one function.
    // -----------------------------------------------------------------------

    /// Run the decision against the scripted catalog. Returns what would be
    /// sent to `model/set` (`None` = send nothing), everything rendered, and how
    /// many questions were asked.
    fn decide(
        name: &str,
        assume_yes: bool,
        answers: &[&str],
    ) -> (Option<ModelSetParams>, RecordingSurface, usize) {
        let list = model_ui::testing::list_result();
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let params = decide_model_set(name, assume_yes, &list, &mut surface, &mut prompter);
        (params, surface, prompter.asked)
    }

    // AC-3b, leg one: a name that fits this machine is sent as-is, with no
    // question and no chatter — `confirmed_above_ram_floor` is false because
    // there was nothing to confirm.
    #[test]
    fn a_fitting_catalog_name_is_sent_without_asking_anything() {
        let name = model_ui::testing::small_entry().name;
        let (params, surface, asked) = decide(&name, false, &[]);

        let params = params.expect("a fitting catalog name should be sent");
        assert_eq!(params.name, name);
        assert!(!params.confirmed_above_ram_floor);
        assert_eq!(asked, 0, "a fitting pick asks nothing");
        assert!(
            surface.calls.is_empty(),
            "a fitting pick warns about nothing: {:?}",
            surface.calls
        );
    }

    // AC-3b, leg two: an unknown name sends nothing and names the alternatives,
    // which is what makes `/model list` unnecessary in-session (spec Out of
    // Scope). Listing them is the whole remedy, so the test asserts every one.
    #[test]
    fn an_unknown_name_sends_nothing_and_lists_the_catalog() {
        let (params, surface, asked) = decide("qwen9-turbo-1t", false, &["y"]);

        assert!(params.is_none(), "an unknown name must not reach model/set");
        assert_eq!(asked, 0, "an unknown name is not a question");
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "one line, not a report: {errors:?}");
        assert!(errors[0].contains("qwen9-turbo-1t"), "{}", errors[0]);
        for model in &model_ui::testing::list_result().models {
            assert!(
                errors[0].contains(&model.entry.name),
                "`{}` is in the catalog but not in the hint: {}",
                model.entry.name,
                errors[0]
            );
        }
    }

    // AC-3b, leg three: above the RAM floor the pick is warned about and only
    // proceeds after an explicit second answer — and only then does
    // `confirmed_above_ram_floor` ride the wire (REQ-547 BR-3).
    #[test]
    fn an_above_floor_name_is_sent_only_after_the_second_confirmation() {
        let name = model_ui::testing::oversized_entry().name;
        let (params, surface, asked) = decide(&name, false, &["y"]);

        let params = params.expect("an explicit yes should send the change");
        assert_eq!(params.name, name);
        assert!(
            params.confirmed_above_ram_floor,
            "the daemon refuses the change without this flag"
        );
        assert_eq!(asked, 1, "exactly one second confirmation");
        assert!(
            surface.any_line_contains(LineKind::Notice, "warning:"),
            "the pick was sent with no warning shown: {:?}",
            surface.calls
        );
        assert!(surface.any_line_contains(LineKind::Notice, &name));
    }

    // AC-3b, leg three's other half — the one that matters. Declining leaves the
    // selection alone, says so, and sends NOTHING: `None` here is the whole
    // guard, since `apply_model_set` issues `model/set` only for `Some`.
    // (LESSON-470: the dialogue defaults to no, so silence declines too.)
    #[test]
    fn declining_the_ram_floor_warning_sends_nothing_and_says_so() {
        let name = model_ui::testing::oversized_entry().name;

        for answer in ["n", "no", ""] {
            let (params, surface, asked) = decide(&name, false, &[answer]);
            assert!(
                params.is_none(),
                "`{answer}` was read as consent and the change was sent"
            );
            assert_eq!(asked, 1, "the decline consumed one dialogue prompt");
            assert!(
                surface.any_line_contains(LineKind::Notice, "selection unchanged"),
                "declining said nothing: {:?}",
                surface.calls
            );
            assert!(surface.any_line_contains(LineKind::Notice, &name));
        }

        // EOF (Ctrl-D at the question) is not consent either.
        let (params, _, asked) = decide(&name, false, &[]);
        assert!(params.is_none(), "EOF was read as consent");
        assert_eq!(asked, 1);
    }

    // The Permissions-table stand-in: `--yes` supplies the second confirmation
    // for an unattended run, and reads no input at all (REQ-547 BR-5's posture).
    // In-session this is the session's own `--yes`, carried on
    // `UiContext::auto_accept_model`.
    #[test]
    fn yes_supplies_the_second_confirmation_and_reads_no_input() {
        let name = model_ui::testing::oversized_entry().name;
        let (params, surface, asked) = decide(&name, true, &["n"]);

        let params = params.expect("--yes should supply the confirmation");
        assert!(params.confirmed_above_ram_floor);
        assert_eq!(
            asked, 0,
            "--yes reads no input, so the scripted `n` is unused"
        );
        assert!(
            surface.any_line_contains(LineKind::Notice, "--yes"),
            "an unattended above-floor install happened silently: {:?}",
            surface.calls
        );
    }
}
