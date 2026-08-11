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
    CategoryBindingConfig, ConfigGetParams, ConfigSetParams, ConfigSnapshot, ConfigUpdate,
    ContentClass, CostQueryParams, CostQueryResult, CostReportView, ModelListParams,
    ModelListResult, ModelSetParams, ModelStatusParams, ModelStatusResult, PrivacyBoundaryConfig,
    ProviderConfig, SessionCreateParams, TierBindingConfig,
};
use teton_protocol::{
    BindingSource, Category, ConfigurableCategory, PrivacyMode, ProviderId, ProviderKind,
    SessionMode, Tier, TierBindingSource,
};

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
use render::{stdout_surface, stdout_surface_with_color, LineKind, Surface};
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

/// `teton policy …` (REQ-558 ADR-H)
#[derive(Debug, Subcommand)]
enum PolicyAction {
    /// Route a tier to a provider — the setting most users want. Every category
    /// on that tier follows unless it has its own override.
    SetTier {
        /// The tier to bind.
        #[arg(value_enum)]
        tier: CliTier,
        /// Provider id to route the tier to.
        provider: String,
        /// Provider used on error/timeout of the primary.
        #[arg(long)]
        fallback: Option<String>,
    },
    /// Route one category to a provider, ahead of its tier's binding.
    SetCategory {
        /// The category to bind: title, digest, compact, triage, edit, shell,
        /// design, debug, or review. `route` and `redact` are pinned to the
        /// local tier by construction and cannot be bound.
        #[arg(value_name = "CATEGORY", value_parser = parse_cli_category)]
        category: CliCategory,
        /// Provider id to route the category to.
        provider: String,
        /// Provider used on error/timeout of the primary.
        #[arg(long)]
        fallback: Option<String>,
    },
    /// Show the effective routing table: every tier, every category, and where
    /// each one resolves right now.
    Show,
    /// The retired phase form. Hidden, and it only explains itself — clap's
    /// "unrecognized subcommand" would tell a user with muscle memory that they
    /// mistyped, when what actually happened is that the dispatch axis changed
    /// underneath them (AC-9).
    #[command(hide = true)]
    Set {
        /// Whatever followed `policy set`; never interpreted.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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

// `CliPhase` retired with `policy set <phase>` (REQ-558 AC-9). Lifecycle phase
// is still a real concept — it gates a structured session and attributes cost —
// but it is not something the CLI asks a user to type at a *routing* command any
// more, and nothing else took an argument of that type.

/// CLI mirror of [`Tier`] — the primary routing surface.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliTier {
    /// Sub-second, every turn, never leaves the machine.
    Reflex,
    /// Read a lot, emit a little.
    Scan,
    /// The agentic loop: read → edit → run → verify.
    Build,
    /// Design, debug, critique.
    Think,
}

impl From<CliTier> for Tier {
    fn from(tier: CliTier) -> Self {
        match tier {
            CliTier::Reflex => Tier::Reflex,
            CliTier::Scan => Tier::Scan,
            CliTier::Build => Tier::Build,
            CliTier::Think => Tier::Think,
        }
    }
}

/// CLI mirror of [`ConfigurableCategory`] — the **nine** a user may bind.
///
/// `redact` and `route` are absent here for the same reason they are absent from
/// the wire type and from the config schema (ADR-B): the CLI does not offer, and
/// cannot construct, a binding that BR-4 and BR-5 forbid.
#[derive(Debug, Clone, Copy)]
struct CliCategory(ConfigurableCategory);

/// Parse a category argument, naming the pin when a user types a pinned one.
///
/// Deliberately **not** a `ValueEnum`: clap would reject `redact` with
/// "invalid value 'redact' … [possible values: title, digest, …]", which reads
/// like a typo. AC-4's criterion is that a user who names a pinned category
/// learns it is *forbidden*, and that sentence comes from the protocol's
/// `FromStr` rather than being written a third time here.
fn parse_cli_category(name: &str) -> Result<CliCategory, String> {
    name.parse::<ConfigurableCategory>()
        .map(CliCategory)
        .map_err(|e| e.to_string())
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
            PolicyAction::SetTier {
                tier,
                provider,
                fallback,
            } => run_policy_set_tier(&paths, tier.into(), provider, fallback),
            PolicyAction::SetCategory {
                category,
                provider,
                fallback,
            } => run_policy_set_category(&paths, category.0, provider, fallback),
            PolicyAction::Show => run_policy_show(&paths),
            PolicyAction::Set { .. } => Err(anyhow::anyhow!(POLICY_SET_RETIRED)),
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
    let mut status_rows = paint_status(ctx, *tick);
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
            status_rows = paint_status(ctx, *tick);
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
                // Two rows up: the layout is [web?][indicator][top rule][input
                // row ← cursor][bottom rule]. The indicator sits directly above
                // the top rule whether or not a web row precedes it, so the
                // offset is unchanged — but the row *count* is not, and it is
                // what `erase` takes back.
                ctx.surface
                    .repaint_row_above(STATUS_ROWS_ABOVE_CURSOR, LineKind::Notice, &line);
                status_rows = usize::from(ctx.state.web.is_engaged()) + 1;
            }
        }
    }
}

/// Draw the status area above the entry frame, returning how many rows it
/// occupies — which is what [`prompt::FramedStdinPrompter::erase`] takes back.
///
/// Two rows at most, in this order: the session's web capability (REQ-563 BR-7)
/// and then REQ-556's loading indicator. The indicator stays **last** so it
/// remains directly above the frame's top rule, which is the geometry
/// [`STATUS_ROWS_ABOVE_CURSOR`] encodes for its in-place animation repaint.
///
/// The web row is drawn only when the capability is engaged — never on a machine
/// that has not opted in, which is every machine by default (BR-1). A session
/// that never touches the web therefore sees the layout it always saw, and this
/// row is not a permanent `web: off` reminder of a feature nobody turned on.
fn paint_status(ctx: &mut UiContext<'_>, tick: u64) -> usize {
    let mut rows = 0;
    if ctx.state.web.is_engaged() {
        let field = ctx.state.web.status_field();
        ctx.surface.line(LineKind::Notice, field);
        rows += 1;
    }
    rows + paint_indicator(ctx, tick)
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

/// The headline a prompt refused because its session is already running a turn
/// renders under (REQ-567 BR-5).
///
/// The daemon's sentence follows and names the turn that holds the session. Like
/// the warming headline above it, this says only the part that decides whether
/// to read the rest: nothing broke and nothing needs fixing, the session is
/// simply busy and the prompt can be sent again.
const SESSION_BUSY_HEADLINE: &str = "session busy —";

/// Render a `prompt/turn` the daemon answered with an error.
///
/// Two classes, told apart by the daemon's own code rather than by re-reading
/// its sentence here (BUG-152). A refusal that **resolves on its own** — a tier
/// still coming up (weights downloading, or loaded and being benchmarked), or a
/// session already running another turn (REQ-567 BR-5) — is not a failure:
/// nothing broke, the user has nothing to fix, and the state ends by itself. It
/// renders as a [`LineKind::Notice`], the same class as the startup lifecycle
/// lines it is a continuation of. Everything else is a real failure and keeps
/// the error line and its `prompt failed:` prefix.
///
/// Split out of the entry loop so both arms are testable without a socket, for
/// the reason [`cost_report_or_report`] is: the arms *are* the behaviour, and a
/// branch only an e2e can reach is a branch that gets asserted on by accident.
fn render_turn_failure(err: &RpcError, surface: &mut dyn Surface) {
    // Matched on the code, never on the sentence: the daemon classified this
    // state once and a client re-deriving it from keywords would be a second
    // classifier for one fact (LESSON-456).
    let headline = match err.code {
        error_code::TIER_WARMING => Some(TIER_WARMING_HEADLINE),
        error_code::SESSION_BUSY => Some(SESSION_BUSY_HEADLINE),
        _ => None,
    };
    match headline {
        Some(headline) => surface.line(LineKind::Notice, &format!("{headline} {}", err.message)),
        None => surface.line(LineKind::Error, &format!("prompt failed: {}", err.message)),
    }
}

/// The default experience: an interactive freeform session (AC-1).
///
/// This is the client that owns the first-run model prompt: it answers permission
/// requests and model proposals, and `auto_accept` (`--yes`) makes the latter
/// unattended (BR-5).
fn run_session(paths: &DaemonPaths, auto_accept: bool, verbose: bool) -> anyhow::Result<()> {
    // The banner is for humans at a terminal. Piped stdout (the e2e suites,
    // shell composition) sees the same byte stream it always did.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Colour is the surface's to apply, so it is decided before the surface
    // exists — the banner names its line classes and the surface draws them.
    let color = interactive && banner::color_enabled();

    let mut surface = stdout_surface_with_color(color);
    let mut state = SessionState::new();
    state.verbose = verbose;
    let mut prompter = StdinPrompter::new();

    // The *other* half of "interactive", read once here at the edge and carried
    // on the context (REQ-555): where the entry lines come from, which is what
    // the `/model set` gate turns on. Two different questions — a session may
    // well have a piped stdin and a terminal stdout — so neither flag is
    // derivable from the other, and a handler must never read either itself.
    let typed_input = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if interactive {
        banner::print(
            &mut surface,
            env!("CARGO_PKG_VERSION"),
            banner::cwd_display().as_deref(),
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
            // Filled in below, the moment `session/create` answers: until then
            // there is no session for a command to act on, and `None` is that
            // fact rather than a placeholder.
            session_id: None,
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
        // The slash handlers act on this session and reach it only through the
        // context (REQ-563: `/web allow` names the session whose restriction it
        // lifts), and the renderer needs it to tell this session's events from
        // another session's on the daemon-wide bus (REQ-567 BR-8).
        ctx.session_id = Some(session_id.clone());
        ctx.state.session_id = Some(session_id.clone());
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
                Err(err) => {
                    render_turn_failure(&err, ctx.surface);
                }
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
        session_id: None,
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
            // The commonest cause is a daemon left running across an upgrade,
            // and `handshake` has already turned that into a sentence with the
            // restart command in it — doctor adds the context that the daemon
            // is up, which is the part its other arms establish.
            Err(err) => surface.line(
                LineKind::Error,
                &format!("daemon: reachable, but it rejected this CLI — {err}"),
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
    // BUG-155 / REQ-557 AC-1: "registering a third with id `opus` fails."
    //
    // It did not. The daemon's `RegisterProvider` is replace-or-insert, so a
    // second `provider add opus --model claude-sonnet-5` silently OVERWROTE the
    // Opus entry and every route the user believed went to Opus went to Sonnet —
    // with no error, and `provider list` showing one provider where they expected
    // two. That is the exact command BR-3's headline ("Opus for design, Sonnet
    // for build") invites people to run twice.
    //
    // The upsert stays: `config/set` is also how a provider is legitimately
    // *updated*, and there is no remove op to sequence around. So the refusal
    // lives here, at the `add` verb, which is what the AC is written about — and
    // it runs before `read_secret` for the same reason the `--model` check does.
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    {
        let mut state = SessionState::new();
        let mut prompter = StdinPrompter::new();
        let mut probe_ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
        if let Ok(cfg) = conn.call(ConfigGetParams::default(), &mut probe_ctx)? {
            if cfg.snapshot.providers.iter().any(|p| p.id.0 == id) {
                anyhow::bail!(
                    "provider `{id}` is already registered. Ids are unique — pick a different \
                     one (e.g. `{id}-2`) if you want a second provider, which is how one vendor \
                     serves two models. Nothing was changed and no credential was read."
                );
            }
        }
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

/// What `teton policy set <phase> <provider>` says now that it is gone (AC-9).
///
/// The sentence names the *reason* rather than only the replacement, because the
/// argument a user is about to retype is a phase, and no amount of "did you
/// mean" gets them to the right tier if they still think in lifecycle position.
/// It deliberately does not map their phase to a tier for them: the CLI holds no
/// routing logic (BR-4), and a phase→tier table written here to be helpful would
/// be a second copy of `categories_for_phase` (ADR-F).
const POLICY_SET_RETIRED: &str = "`teton policy set <phase> <provider>` is retired. Routing \
     dispatches on what a call is *for* — classify, summarize, edit, critique — not on where in \
     the lifecycle it happens, so a phase is no longer something to route. Bind a tier with \
     `teton policy set-tier <reflex|scan|build|think> <provider>`, or one category with \
     `teton policy set-category <category> <provider>`. `teton policy show` prints the whole \
     table, including which tier each category inherits from.";

/// `teton policy set-tier`.
fn run_policy_set_tier(
    paths: &DaemonPaths,
    tier: Tier,
    provider: String,
    fallback: Option<String>,
) -> anyhow::Result<()> {
    run_policy_bind(
        paths,
        ConfigUpdate::SetTierBinding(TierBindingConfig {
            tier,
            provider_id: ProviderId::from(provider.as_str()),
            fallback_id: fallback.as_deref().map(ProviderId::from),
        }),
        &format!("the '{tier}' tier"),
        &provider,
        fallback.as_deref(),
    )
}

/// `teton policy set-category`.
fn run_policy_set_category(
    paths: &DaemonPaths,
    category: ConfigurableCategory,
    provider: String,
    fallback: Option<String>,
) -> anyhow::Result<()> {
    run_policy_bind(
        paths,
        ConfigUpdate::SetCategoryBinding(CategoryBindingConfig {
            name: category,
            provider_id: ProviderId::from(provider.as_str()),
            fallback_id: fallback.as_deref().map(ProviderId::from),
        }),
        &format!("the '{category}' category"),
        &provider,
        fallback.as_deref(),
    )
}

/// The shared body of `set-tier` and `set-category`: one round trip, one set of
/// outcomes, one sentence shape. The two differ only in what they bind.
fn run_policy_bind(
    paths: &DaemonPaths,
    update: ConfigUpdate,
    what: &str,
    provider: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let fallback_note = fallback.map_or_else(String::new, |f| format!(" (fallback {f})"));
    match conn.call(ConfigSetParams { update }, &mut ctx)? {
        Ok(res) if res.applied => ctx.surface.line(
            LineKind::Info,
            &format!("{what} now routes to `{provider}`{fallback_note}."),
        ),
        Ok(_) => ctx.surface.line(
            LineKind::Notice,
            &format!("the daemon did not apply the binding for {what}."),
        ),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not implement config/set yet (wiring in progress).",
        ),
        // The daemon screens the provider before it writes anything (REQ-557
        // BR-6, BUG-155 M4's shape), so its message already names what went
        // wrong and what is registered. Passing it through beats paraphrasing.
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("{what} was not bound: {}", err.message),
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
        Ok(cfg) => render_policy(&cfg.snapshot, ctx.surface),
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

/// Render `teton policy show` (REQ-558 ADR-A, ADR-H).
///
/// **Every routing fact here was decided by the daemon's resolver** — the same
/// `category::resolve` a turn goes through, with the same live provider health
/// (BR-6, AC-11). This function chooses column widths and English connectives
/// and nothing else. That restraint is the whole design: `policy show` is the
/// surface most tempting to "just format nicely" with logic of its own, and a
/// table that computed its own answer would be a second routing implementation
/// that only a human ever compares against the first.
fn render_policy(snapshot: &ConfigSnapshot, surface: &mut dyn Surface) {
    if snapshot.tiers.is_empty() && snapshot.routing.is_empty() {
        surface.line(
            LineKind::Notice,
            "this daemon build reports no routing table.",
        );
        return;
    }

    surface.line(
        LineKind::Info,
        "tiers — the primary surface; a category follows its tier unless overridden:",
    );
    let tier_width = snapshot
        .tiers
        .iter()
        .map(|t| t.tier.as_str().len())
        .max()
        .unwrap_or(0);
    for row in &snapshot.tiers {
        let target = match &row.provider_id {
            Some(id) => format!("→ {id}"),
            None => "— nothing bound and nothing to inherit".to_owned(),
        };
        let fallback = row
            .fallback_id
            .as_ref()
            .map_or_else(String::new, |f| format!(" (fallback {f})"));
        surface.line(
            LineKind::Info,
            &format!(
                "  {:<tier_width$}  {target}{fallback}  [{}]",
                row.tier.as_str(),
                tier_origin_label(row.source),
            ),
        );
    }

    surface.line(LineKind::Info, "categories:");
    let category_width = snapshot
        .routing
        .iter()
        .map(|c| c.category.as_str().len())
        .max()
        .unwrap_or(0);
    let provider_width = snapshot
        .routing
        .iter()
        .filter_map(|c| c.provider_id.as_ref().map(|p| p.0.len()))
        .max()
        .unwrap_or(0);
    for row in &snapshot.routing {
        // REQ-562: `redact` is the one category whose call site can be live and
        // which still transmits nothing — what decides is the `[privacy]`
        // switch, not its binding. So its verb follows the switch (never
        // "sends" while nothing scans) and its row states the switch outright.
        let transmits_today = row.category != Category::Redact || snapshot.redact_enabled;
        let switch = redact_switch_note(row.category, snapshot.redact_enabled);
        // An unresolvable category carries the resolver's sentence instead of a
        // blank column: BR-8 requires it to name itself and its unset binding,
        // and the sentence that does so already exists.
        let Some(provider) = &row.provider_id else {
            surface.line(
                LineKind::Notice,
                &format!(
                    "  {:<category_width$}  {:<6}  unresolved — {}{}{switch}",
                    row.category.as_str(),
                    row.tier.as_str(),
                    row.reason,
                    content_disclosure(row.content_class, row.reached, false),
                ),
            );
            continue;
        };
        let fallback = row
            .fallback_id
            .as_ref()
            .map_or_else(String::new, |f| format!(" (fallback {f})"));
        surface.line(
            LineKind::Info,
            &format!(
                "  {:<category_width$}  {:<6}  → {:<provider_width$}  [{}]{fallback}{}{switch}",
                row.category.as_str(),
                row.tier.as_str(),
                provider.0,
                binding_source_label(row.source),
                content_disclosure(row.content_class, row.reached, transmits_today),
            ),
        );
    }

    // AC-12: the BR-9 declared default, on the surface a user reads to find out
    // where a turn will go. A freeform turn the classifier cannot categorize —
    // or does not run for at all, because the local tier cannot serve it — lands
    // here, so it is part of the routing answer, not a footnote.
    if let Some(default) = snapshot.judgment_default {
        surface.line(
            LineKind::Info,
            &format!(
                "a freeform turn whose category the `route` classifier does not decide is \
                 treated as `{default}` (judgment_default)."
            ),
        );
    }
}

/// Label for where an unbound tier's provider came from.
fn tier_origin_label(source: TierBindingSource) -> &'static str {
    match source {
        TierBindingSource::Configured => "configured",
        TierBindingSource::DefaultProvider => "unbound; inherits default_provider",
        TierBindingSource::LocalTier => "unbound; inherits the local tier",
        TierBindingSource::Unbound => "unbound",
    }
}

/// What a category transmits, and whether it transmits anything today (REQ-561
/// BR-11, AC-16 — OQ-4's resolution).
///
/// The disclosure exists because the `scan` tier carries both `triage` (grep
/// match text — file content) and `compact` (conversation blocks): a user who
/// binds `scan` remotely for cheap long-context work also moves conversation
/// history off the machine. Re-splitting that binding is REQ-558's decision and
/// out of scope, so legibility is the mitigation — which only works if the two
/// rows visibly disclose *different* things.
///
/// **One string rather than two columns, because the two facts are only honest
/// together.** [`ContentClass`] is what a category is *for*, and it is declared
/// for all eleven including the ones with no call site — "what would leave this
/// machine if I bound that tier remotely?" has an answer before the call site
/// exists, and a blank cell would read as "this one is safe". But a class
/// printed on its own reads as a live egress path, which a declared-but-uncalled
/// category's is not. So the verb carries `reached`: a category with no call
/// site *would* send, and says in the same breath that nothing calls it yet.
/// ADR-A's marker stays exactly where a reader needs it — next to the claim it
/// qualifies. (All eleven are reached as of REQ-562, which wired `redact` last;
/// the `reached: false` arm is for the next category to land declared before it
/// is called.)
///
/// `transmits_today` is the third fact and the reason an unresolved row does not
/// claim to send: a category whose tier is unbound transmits nothing today
/// either, and its row's own sentence already says why. **`redact` is the second
/// way it can be false** (REQ-562): its call site is live and its binding
/// resolves, and it still scans nothing while `[privacy] redact` is off — so the
/// present tense would be a claim the daemon is doing something it is not, which
/// is exactly the report-honesty AC-13 asks for on the other surfaces.
/// [`redact_switch_note`] says which state the row is in, because the verb alone
/// cannot distinguish "the switch is off" from "the binding is missing".
///
/// **Disclosure, not a control.** Nothing here refuses anything. BR-7's
/// per-content egress scoping is the enforcement, and it refuses a `local-only`
/// source whatever this line says.
fn content_disclosure(class: ContentClass, reached: bool, transmits_today: bool) -> String {
    match (reached, transmits_today) {
        (true, true) => format!("  — sends {}", class.describe()),
        (true, false) => format!("  — would send {}", class.describe()),
        (false, _) => format!(
            "  — would send {}; declared, no call site yet",
            class.describe()
        ),
    }
}

/// Whether the redaction scan runs, for the one row it is about (REQ-562).
///
/// Empty for every other category — this is a fact about `redact` and nothing
/// else, and a column repeating "n/a" ten times would be noise on the surface a
/// user reads to answer one question.
///
/// **Why the row states it at all.** Every other category's row answers "where
/// does this go?", and for `redact` the prior question is whether it goes
/// anywhere: the switch defaults to off (BR-10/OQ-3), off means the gate is
/// never installed rather than installed-and-permissive (ADR-2), and a row that
/// named a provider and a class while nothing scanned would read as a live
/// scan. So the disabled wording names the default *and* the key that changes
/// it — a user who wanted the scan and does not have it is one line away from
/// the fix, without leaving the surface that told them.
///
/// Read off the wire, like every other fact in this table: the daemon answers,
/// this function chooses the English (see [`render_policy`]).
fn redact_switch_note(category: Category, enabled: bool) -> &'static str {
    if category != Category::Redact {
        return "";
    }
    if enabled {
        "; content scan: enabled"
    } else {
        "; content scan: disabled (default — enable with `[privacy] redact = true`)"
    }
}

/// Label for which row supplied a category's binding.
fn binding_source_label(source: BindingSource) -> &'static str {
    match source {
        BindingSource::Override => "per-category override",
        BindingSource::TierInheritance => "via its tier",
        BindingSource::PinnedLocal => "pinned local, not configurable",
        BindingSource::Unbound => "unbound",
    }
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

    /// A snapshot shaped like the one a REQ-557-migrated install produces:
    /// `default_provider` on a remote provider, so `scan`/`build`/`think`
    /// inherit it and `reflex` does not.
    ///
    /// **Its `reached` column mirrors the daemon's `has_call_site` by hand**, and
    /// nothing in this crate can check that: the CLI is a thin client and holds
    /// no dependency on `tetond` (that separation is BR-4's, and worth more than
    /// this convenience). So the fixture can go stale while every test over it
    /// stays green — which is exactly what happened through TASK-060..063, when
    /// four categories got call sites and this fixture went on calling them
    /// unreached (LESSON-485: a rendering test fed an impossible snapshot still
    /// passes). What the daemon actually reaches is derived and asserted in
    /// `tetond`'s `the_unreached_marker_matches_the_daemons_actual_call_sites`,
    /// and the real binary's rendering of it in `cli_e2e`'s
    /// `policy_show_renders_the_daemons_resolved_table`. This fixture's job is
    /// narrower: prove the renderer prints whatever it is handed.
    fn migrated_snapshot() -> ConfigSnapshot {
        use teton_protocol::methods::{CategoryRouteView, ContentClass, TierRouteView};
        use teton_protocol::Category;

        let tier = |t: Tier, provider: &str, source| TierRouteView {
            tier: t,
            provider_id: Some(ProviderId::from(provider)),
            fallback_id: None,
            source,
        };
        let row =
            |category: Category, t: Tier, provider: &str, source, reached| CategoryRouteView {
                category,
                tier: t,
                provider_id: Some(ProviderId::from(provider)),
                fallback_id: None,
                source,
                reached,
                content_class: ContentClass::for_category(category),
                reason: format!("Routing the '{category}' category to '{provider}'."),
            };
        use BindingSource::{PinnedLocal, TierInheritance as Inherit};
        use TierBindingSource::{DefaultProvider, LocalTier};
        ConfigSnapshot {
            providers: Vec::new(),
            tiers: vec![
                tier(Tier::Reflex, "on-device", LocalTier),
                tier(Tier::Scan, "anthropic", DefaultProvider),
                tier(Tier::Build, "anthropic", DefaultProvider),
                tier(Tier::Think, "anthropic", DefaultProvider),
            ],
            routing: vec![
                row(
                    Category::Route,
                    Tier::Reflex,
                    "on-device",
                    PinnedLocal,
                    true,
                ),
                // The one category REQ-561 leaves unwired: a model call inside
                // the egress choke point is REQ-562's subject and its own
                // adversarial review. Every other row here is `true` because
                // TASK-060..063 gave the other four call sites.
                row(
                    Category::Redact,
                    Tier::Reflex,
                    "on-device",
                    PinnedLocal,
                    false,
                ),
                row(Category::Title, Tier::Reflex, "on-device", Inherit, true),
                row(Category::Digest, Tier::Scan, "anthropic", Inherit, true),
                row(Category::Compact, Tier::Scan, "anthropic", Inherit, true),
                row(Category::Triage, Tier::Scan, "anthropic", Inherit, true),
                row(Category::Edit, Tier::Build, "anthropic", Inherit, true),
                row(Category::Shell, Tier::Build, "anthropic", Inherit, true),
                row(Category::Design, Tier::Think, "anthropic", Inherit, true),
                row(Category::Debug, Tier::Think, "anthropic", Inherit, true),
                // One override, on the same tier as `design` and `debug`, so
                // "which row supplied this?" is not recoverable from the tier or
                // the provider — only from `source`.
                row(
                    Category::Review,
                    Tier::Think,
                    "anthropic",
                    BindingSource::Override,
                    true,
                ),
            ],
            judgment_default: Some(Category::Edit),
            privacy: Vec::new(),
            // The default posture (BR-10/OQ-3). The tests that care about the
            // switch set it explicitly, in both states.
            redact_enabled: false,
        }
    }

    /// The rendered row whose first word is `category`.
    ///
    /// Matched on the row prefix rather than by bare substring, so `route` is not
    /// satisfied by the word "Routing" inside somebody else's reason. Every name
    /// is padded to the widest, so a trailing space always follows it.
    fn category_row<'a>(rendered: &'a str, category: &str) -> &'a str {
        let prefix = format!("{category} ");
        rendered
            .lines()
            .map(str::trim_start)
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no row for {category}:\n{rendered}"))
    }

    /// AC-1 + ADR-A + AC-12: the table a human reads names every category, marks
    /// any with no call site, and states the BR-9 default.
    ///
    /// **AC-1 is "the marker becomes accurate", not "delete the marker."** The
    /// `redact` assertion below is what keeps that distinction load-bearing: a
    /// renderer that had simply forgotten how to print the marker would satisfy
    /// every `!contains("no call site")` in this file and fail only there.
    ///
    /// Since REQ-562 TASK-070 wired `redact`, **no real category is unreached**
    /// — so the `reached: false` row is a property of this fixture rather than
    /// of the daemon, and deliberately kept. This is now the only layer at which
    /// the marker can be exercised at all (the e2e half of the pair had to give
    /// it up when the derived set emptied), which makes it more load-bearing
    /// than before, not less. Nothing derives `reached` here; the daemon sends
    /// it, and `tetond`'s own tests hold the daemon to its call sites.
    #[test]
    fn policy_show_marks_the_unreached_categories_and_the_judgment_default() {
        let mut surface = RecordingSurface::new();
        render_policy(&migrated_snapshot(), &mut surface);
        let rendered = surface.lines_of(LineKind::Info).join("\n");

        // The fixture's unreached row. A live daemon marks none of the eleven
        // today; a twelfth arriving unwired would be marked exactly like this.
        let unreached = category_row(&rendered, "redact");
        assert!(
            unreached.contains("declared, no call site yet"),
            "a row the daemon sent as unreached must say so: {unreached}"
        );
        // The four REQ-561 wired (TASK-060..063) join the six that were already
        // reached. `triage`, `shell`, `title` and `compact` were on the marked
        // side of this very assertion until their duties landed.
        for reached in [
            "route", "digest", "edit", "design", "debug", "review", "triage", "shell", "title",
            "compact",
        ] {
            let line = category_row(&rendered, reached);
            assert!(
                !line.contains("no call site"),
                "{reached} is reached and must not be marked: {line}"
            );
        }

        // BR-6 / AC-11: the "where did this binding come from" column is the
        // resolver's `source`, and nothing else.
        //
        // The fixture is built so that no other field can stand in for it:
        // `title` shares a provider with the two pinned rows but is inherited,
        // and `review` shares a tier *and* a provider with `design` but is an
        // override. A renderer that recovered the column from the provider id,
        // the tier, or the reason sentence would mislabel one of those three —
        // which is the whole failure mode this assertion exists for.
        for (category, expected) in [
            ("route", "pinned local, not configurable"),
            ("redact", "pinned local, not configurable"),
            ("title", "via its tier"),
            ("design", "via its tier"),
            ("review", "per-category override"),
        ] {
            let line = category_row(&rendered, category);
            assert!(
                line.contains(expected),
                "{category} must be labelled `{expected}`: {line}"
            );
            for wrong in [
                "pinned local, not configurable",
                "via its tier",
                "per-category override",
            ] {
                assert!(
                    wrong == expected || !line.contains(wrong),
                    "{category} is labelled `{wrong}` as well as `{expected}`: {line}"
                );
            }
        }

        // AC-12.
        assert!(rendered.contains("judgment_default"), "{rendered}");
        assert!(rendered.contains("`edit`"), "{rendered}");

        // The tier table names the fill an unbound tier takes, and `reflex`
        // differs — the asymmetry a user would otherwise have to infer.
        assert!(rendered.contains("inherits the local tier"), "{rendered}");
        assert!(rendered.contains("inherits default_provider"), "{rendered}");
    }

    /// AC-16 / BR-11: every one of the eleven rows names the content class it
    /// transmits, and `triage` and `compact` name **different** ones despite
    /// sharing the `scan` tier.
    ///
    /// That distinctness is the whole of OQ-4's resolution. Re-splitting the
    /// tier→category bindings is REQ-558's decision and out of scope, so a user
    /// who binds `scan` remotely for cheap long-context work is told, in the two
    /// rows they would compare, that they have moved both file text *and*
    /// conversation history off the machine. If both rows said the same thing,
    /// the mitigation would disclose nothing.
    ///
    /// Disclosure only: nothing here is a control, and no assertion in this test
    /// claims one. BR-7's per-content egress scoping is the enforcement.
    #[test]
    fn policy_show_discloses_the_content_class_of_every_category() {
        let snapshot = migrated_snapshot();
        let mut surface = RecordingSurface::new();
        render_policy(&snapshot, &mut surface);
        let rendered = surface.lines_of(LineKind::Info).join("\n");

        assert_eq!(
            snapshot.routing.len(),
            11,
            "AC-16 is about all eleven categories, so the fixture must carry all eleven"
        );
        for row in &snapshot.routing {
            let name = row.category.as_str();
            let line = category_row(&rendered, name);
            assert!(
                line.contains(row.content_class.describe()),
                "`{name}` must name what it transmits (`{}`): {line}",
                row.content_class.describe()
            );
        }

        // The asymmetry itself, spelled out rather than derived — a change to
        // either category's class should have to be made here too, in front of a
        // reviewer, and not slide through a `for_category` round trip.
        let triage = category_row(&rendered, "triage");
        let compact = category_row(&rendered, "compact");
        assert!(
            triage.contains("file content and your request"),
            "`triage` ranks grep hits — file text — and is told the request and the search \
             terms in the same prompt, so its row must name all of it: {triage}"
        );
        assert!(
            compact.contains("conversation history"),
            "`compact` decides which conversation blocks to forget, so it reads them: {compact}"
        );
        assert!(
            !triage.contains("conversation history"),
            "`triage` must not disclose `compact`'s class: {triage}"
        );
        assert!(
            !compact.contains("file content"),
            "`compact` must not disclose `triage`'s class: {compact}"
        );
        // The type-level half of this — that the mapping itself answers two
        // different classes — is pinned in `teton-protocol`'s
        // `triage_and_compact_disclose_different_content_despite_sharing_a_tier`.
        // What is asserted here is that the *rendering* keeps them apart, which
        // is the half a user actually reads.
    }

    /// The row prints the **daemon's** content class rather than recomputing one.
    ///
    /// [`CategoryRouteView`]'s own doc says the daemon answers and the surface
    /// prints; [`render_policy`]'s says a table that computed its own answer
    /// would be a second implementation only a human ever compares against the
    /// first. For the routing columns that is structural — this crate has no
    /// resolver to recompute them with. For `content_class` it is **not**:
    /// `ContentClass::for_category` lives in the shared protocol crate and this
    /// renderer could simply call it, ignoring the wire while passing every
    /// other assertion in this file, because the two agree by construction.
    ///
    /// Found by mutation: swapping `row.content_class` for
    /// `ContentClass::for_category(row.category)` came back green across the unit
    /// tests *and* the e2e until this test existed. It matters because REQ-562
    /// will change what `redact` transmits — and a CLI that answers from its own
    /// compiled-in copy disagrees with the daemon the moment the two are built
    /// from different versions, which is the drift ADR-002's TypeScript mirror
    /// note is about.
    #[test]
    fn policy_show_prints_the_daemons_content_class_rather_than_recomputing_it() {
        use teton_protocol::Category;

        let mut snapshot = migrated_snapshot();
        // A class no local derivation would produce for `triage`.
        let wire = ContentClass::CommandOutput;
        assert_ne!(wire, ContentClass::for_category(Category::Triage));
        for row in &mut snapshot.routing {
            if row.category == Category::Triage {
                row.content_class = wire;
            }
        }

        let mut surface = RecordingSurface::new();
        render_policy(&snapshot, &mut surface);
        let rendered = surface.lines_of(LineKind::Info).join("\n");

        let triage = category_row(&rendered, "triage");
        assert!(
            triage.contains(wire.describe()),
            "the row must print the class the daemon sent: {triage}"
        );
        assert!(
            !triage.contains(ContentClass::for_category(Category::Triage).describe()),
            "the class was recomputed from the category instead of read off the wire: {triage}"
        );
    }

    /// AC-16's "a category that transmits nothing today says so": the content
    /// class and the call-site marker render as one adjacent phrase.
    ///
    /// TASK-059 shipped no `Nothing` variant on purpose. `content_class` is what
    /// a category *would* transmit — `redact` carries the outbound payload now
    /// that REQ-562 TASK-070 has wired it, so calling it `Nothing` would have
    /// been a lie with a short shelf life — and `reached` is whether anything
    /// transmits it today. The cost of that (correct) division is that a class
    /// printed on its own reads as a live egress path. This is the assertion
    /// that keeps the two facts from drifting apart in the rendering.
    ///
    /// The fixture below still carries an unreached `redact` row, because the
    /// *rendering* of a conditional class beside its marker has to stay covered
    /// after the last real category was wired.
    #[test]
    fn policy_show_renders_the_content_class_beside_the_call_site_marker() {
        use teton_protocol::Category;

        let mut surface = RecordingSurface::new();
        render_policy(&migrated_snapshot(), &mut surface);
        let rendered = surface.lines_of(LineKind::Info).join("\n");

        // The fixture's unreached row: class and marker in a single phrase, in
        // that order, with nothing between them.
        let redact = category_row(&rendered, "redact");
        let class = ContentClass::for_category(Category::Redact).describe();
        assert!(
            redact.contains(&format!("would send {class}; declared, no call site yet")),
            "`redact`'s class and its marker must read as one phrase, or the class alone reads \
             as a live egress path: {redact}"
        );

        // A wired one, for the other half of the pair: the class is present, the
        // marker is not, and the verb is the one that says the call is live.
        let triage = category_row(&rendered, "triage");
        let wired = ContentClass::for_category(Category::Triage).describe();
        assert!(
            triage.contains(&format!("sends {wired}")),
            "a wired category states its class in the present tense: {triage}"
        );
        assert!(
            !triage.contains("would send"),
            "`triage` has a call site, so its disclosure is not conditional: {triage}"
        );
        assert!(
            !triage.contains("no call site"),
            "`triage` is wired (TASK-060) and must not carry the marker: {triage}"
        );
    }

    /// **The `[privacy]` switch is reported, in both states** (REQ-562; user
    /// decision, 2026-08-08).
    ///
    /// One fixture, one field flipped, two renderings compared — so each claim
    /// is discriminated by its opposite rather than by a lone `contains`
    /// (LESSON-485). A renderer that printed the disabled hint unconditionally,
    /// or that ignored the wire and hard-coded either state, fails one leg.
    ///
    /// Three things are asserted per state, because they fail independently:
    ///
    /// 1. the switch is **named**, and the disabled wording names the key that
    ///    turns it on — a user who wanted the scan is one line from the fix;
    /// 2. the **verb** follows the switch. With the scan off nothing is
    ///    scanned, so "sends the outbound payload" would claim work the daemon
    ///    is not doing (AC-13's report honesty, extended to this surface);
    /// 3. the **pin sentence is intact** in both states. The switch decides
    ///    whether the scan runs, never where it runs — `redact` is pinned local
    ///    and not configurable either way, and a row that lost that while
    ///    gaining a status line would have traded one disclosure for another.
    #[test]
    fn policy_show_reports_whether_the_redaction_scan_runs() {
        use teton_protocol::Category;

        /// The `redact` row as rendered with the switch in `enabled`, from a
        /// fixture whose `redact` row is otherwise **wired** — the live
        /// daemon's shape, and the only one in which the present tense is even
        /// a possibility.
        fn redact_row_with(enabled: bool) -> String {
            let mut snapshot = migrated_snapshot();
            snapshot.redact_enabled = enabled;
            for row in &mut snapshot.routing {
                if row.category == Category::Redact {
                    row.reached = true;
                }
            }
            let mut surface = RecordingSurface::new();
            render_policy(&snapshot, &mut surface);
            let rendered = surface.lines_of(LineKind::Info).join("\n");
            category_row(&rendered, "redact").to_owned()
        }

        let on = redact_row_with(true);
        let off = redact_row_with(false);
        let class = ContentClass::for_category(Category::Redact).describe();

        // 1. The switch, and the way out of the off state.
        assert!(
            on.contains("content scan: enabled") && !on.contains("disabled"),
            "the enabled row must say so, once: {on}"
        );
        assert!(
            off.contains(
                "content scan: disabled (default — enable with `[privacy] redact = true`)"
            ),
            "the disabled row must name the default and the key that changes it: {off}"
        );

        // 2. The verb, which is the honesty half: `sends` iff something scans.
        assert!(
            on.contains(&format!("sends {class}")) && !on.contains("would send"),
            "with the scan on, the row states its class in the present tense: {on}"
        );
        assert!(
            off.contains(&format!("would send {class}")),
            "with the scan off nothing is scanned, so the row must not claim to send: {off}"
        );
        assert!(
            !off.contains("no call site"),
            "the switch being off is not the same fact as having no call site, and \
             the row must not conflate them: {off}"
        );

        // 3. The pin, unchanged by either state.
        for (label, row) in [("on", &on), ("off", &off)] {
            assert!(
                row.contains("pinned local, not configurable"),
                "{label}: the pin sentence must survive the status line: {row}"
            );
        }
    }

    /// BR-8: a category that cannot be routed carries the resolver's sentence
    /// rather than a blank column — and it is a notice, not an info line,
    /// because it is the answer to "why did my turn fail".
    #[test]
    fn policy_show_reports_an_unresolvable_category_with_its_reason() {
        let mut snapshot = migrated_snapshot();
        let reason = "No provider is bound to the 'think' tier and the 'design' category has \
                      no override, so 'design' cannot be routed."
            .to_owned();
        for row in &mut snapshot.routing {
            if row.category == teton_protocol::Category::Design {
                row.provider_id = None;
                row.source = BindingSource::Unbound;
                row.reason.clone_from(&reason);
            }
        }
        let mut surface = RecordingSurface::new();
        render_policy(&snapshot, &mut surface);

        let notices = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notices.contains("design"), "{notices}");
        assert!(
            notices.contains(&reason),
            "the resolver's sentence must travel verbatim: {notices}"
        );

        // AC-16 covers every row, and this is the branch that would silently
        // drop the disclosure: a category that cannot be routed still says what
        // it would transmit. In the conditional, because it transmits nothing
        // while its tier is unbound — and without ADR-A's marker, because what
        // `design` is missing is a binding, not a call site.
        let line = category_row(&notices, "design");
        assert!(
            line.contains("would send the whole turn"),
            "an unroutable category still discloses its content class: {line}"
        );
        assert!(
            !line.contains("declared, no call site yet"),
            "`design` has a call site; it is its binding that is missing: {line}"
        );
        // And the rest of the table still renders.
        assert!(surface.any_line_contains(LineKind::Info, "review"));
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
    fn policy_set_tier_parses_tier_provider_and_fallback() {
        let cli = parse(&[
            "teton",
            "policy",
            "set-tier",
            "think",
            "anthropic",
            "--fallback",
            "deepseek",
        ]);
        match cli.command {
            Some(Command::Policy {
                action:
                    PolicyAction::SetTier {
                        tier,
                        provider,
                        fallback,
                    },
            }) => {
                assert!(matches!(tier, CliTier::Think));
                assert_eq!(Tier::from(tier), Tier::Think);
                assert_eq!(provider, "anthropic");
                assert_eq!(fallback.as_deref(), Some("deepseek"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // Every tier is spellable, `reflex` included: never inheriting a remote
        // default (REQ-557 BR-4) is not the same as never being bindable, and a
        // user who deliberately puts `reflex` on a fast remote model is making a
        // choice the config file can already express.
        for tier in ["reflex", "scan", "build", "think"] {
            parse(&["teton", "policy", "set-tier", tier, "p"]);
        }
    }

    #[test]
    fn policy_set_category_parses_category_provider_and_fallback() {
        let cli = parse(&[
            "teton",
            "policy",
            "set-category",
            "review",
            "deepseek",
            "--fallback",
            "anthropic",
        ]);
        match cli.command {
            Some(Command::Policy {
                action:
                    PolicyAction::SetCategory {
                        category,
                        provider,
                        fallback,
                    },
            }) => {
                assert_eq!(category.0, ConfigurableCategory::Review);
                assert_eq!(provider, "deepseek");
                assert_eq!(fallback.as_deref(), Some("anthropic"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        for category in ConfigurableCategory::ALL {
            parse(&["teton", "policy", "set-category", category.as_str(), "p"]);
        }
    }

    /// AC-4 / BR-4 / BR-5: **two** categories are unsettable, and the CLI must
    /// say *pinned*, not "invalid value".
    ///
    /// Asserted for both rather than for `redact` alone (LESSON-479: a subset
    /// invariant only holds where you iterate), and each must cite its own
    /// reason — `route` is pinned because a remote classifier costs more than
    /// the decision saves, which is not `redact`'s reason at all.
    #[test]
    fn policy_set_category_rejects_a_pinned_category_by_naming_the_pin() {
        for pinned in ["redact", "route"] {
            let err = Cli::try_parse_from(["teton", "policy", "set-category", pinned, "anthropic"])
                .expect_err("a pinned category cannot be bound")
                .to_string();
            assert!(err.contains(pinned), "{pinned}: {err}");
            assert!(err.contains("pinned"), "{pinned}: {err}");
            assert!(
                !err.contains("invalid value") || err.contains("pinned"),
                "{pinned} reads as a typo: {err}"
            );
        }
        let redact = parse_cli_category("redact").expect_err("pinned");
        assert!(redact.contains("leave the machine"), "{redact}");
        let route = parse_cli_category("route").expect_err("pinned");
        assert!(route.contains("classifier"), "{route}");
        assert!(
            !route.contains("leave the machine"),
            "route inherited redact's explanation: {route}"
        );

        // And a genuine typo still reads as a typo, listing only bindable names.
        let typo = parse_cli_category("reviw").expect_err("unknown");
        assert!(typo.contains("reviw") && typo.contains("review"), "{typo}");
        assert!(
            !typo.contains("redact") && !typo.contains("route"),
            "{typo}"
        );
    }

    /// AC-9: the phase form is gone, and says why rather than reading as a typo.
    #[test]
    fn the_retired_phase_form_explains_itself() {
        let cli = parse(&["teton", "policy", "set", "implement", "deepseek"]);
        assert!(matches!(
            cli.command,
            Some(Command::Policy {
                action: PolicyAction::Set { .. }
            })
        ));
        assert!(POLICY_SET_RETIRED.contains("set-tier"));
        assert!(POLICY_SET_RETIRED.contains("set-category"));
        // It names the reason, not only the replacement: a user still thinking
        // in lifecycle position needs to stop, not to retype.
        assert!(POLICY_SET_RETIRED.contains("what a call is *for*"));
        // And it does not pretend to know which tier their phase became — that
        // map lives in `teton-core`, and a copy here would be a second one.
        for phase in ["implement", "architect", "review", "spec", "io"] {
            assert!(
                !POLICY_SET_RETIRED.contains(phase),
                "the retirement notice maps {phase} to a tier; that table is \
                 `categories_for_phase`'s (ADR-F)"
            );
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
                session_id: None,
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

    /// REQ-563 BR-7: the status area gains the session's web capability, and it
    /// costs an opted-out session nothing.
    ///
    /// The row count is what `erase` takes back, so it is asserted alongside the
    /// content: a status area that drew two rows and reported one would shred
    /// the entry frame above it.
    #[test]
    fn the_status_area_shows_the_web_state_and_stays_empty_when_it_is_off() {
        use teton_protocol::events::{
            Event, EventEnvelope, ModelLifecycleStage, WebLookup, WebLookupKind, WebLookupOutcome,
        };

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
            session_id: None,
        };

        // BR-1: a machine that never opted in draws exactly what it drew before
        // this REQ — no row, and no `web: off` reminder of a feature nobody
        // turned on.
        assert_eq!(paint_status(&mut ctx, 0), 0);

        // A lookup that ran proves the tier, and the row appears.
        session_ui::render_event(
            &EventEnvelope::new(
                1,
                None,
                Event::WebLookup(WebLookup {
                    kind: WebLookupKind::Search,
                    host: "search.example".to_owned(),
                    outcome: WebLookupOutcome::Completed,
                    bytes_in: 10,
                    cause: None,
                }),
            ),
            ctx.surface,
            ctx.state,
        );
        assert_eq!(paint_status(&mut ctx, 0), 1);

        // With the loading indicator up too, the area is two rows — and the
        // indicator is still the last one drawn, which is the geometry
        // `STATUS_ROWS_ABOVE_CURSOR` encodes.
        ctx.state.loading.observe(
            "qwen3-coder-30b-a3b",
            &ModelLifecycleStage::Benchmark {
                first_token_ms: 368,
                tokens_per_sec: 73.0,
            },
        );
        assert_eq!(paint_status(&mut ctx, 0), 2);

        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(
            notices.iter().filter(|l| **l == "web: search").count(),
            2,
            "one row from each of the two engaged paints — the first paint, \
             before any lookup, drew nothing at all: {notices:?}"
        );
        let web_at = notices.iter().position(|l| *l == "web: search").unwrap();
        let motion_at = notices
            .iter()
            .rposition(|l| l.contains("model starting"))
            .expect("the indicator drew its row");
        assert!(
            web_at < motion_at,
            "the indicator must stay last, directly above the frame: {notices:?}"
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

    /// REQ-567 BR-5: a prompt refused because the session is already running a
    /// turn is the second self-resolving state, and it renders like the first.
    ///
    /// The assertion that matters is the *turn id surviving*: BR-5 requires a
    /// refusal to name its cause, and a client that dropped the daemon's
    /// sentence in favour of its own headline would leave the user with a
    /// generic "busy" and no way to tell which turn they are waiting on.
    #[test]
    fn a_busy_session_is_a_notice_that_still_names_the_turn_holding_it() {
        let mut surface = RecordingSurface::new();
        render_turn_failure(
            &RpcError::new(
                error_code::SESSION_BUSY,
                "session sess-under-test is already running turn turn-3; one session runs one \
                 turn at a time — retry when it finishes",
            ),
            &mut surface,
        );

        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a busy session is not a broken one: {:?}",
            surface.calls
        );
        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(notices.len(), 1, "exactly one line: {notices:?}");
        assert!(
            notices[0].starts_with(SESSION_BUSY_HEADLINE),
            "the waiting headline leads, so it is what scans: {}",
            notices[0]
        );
        assert!(
            notices[0].contains("turn-3") && notices[0].contains("retry when it finishes"),
            "the daemon's reason — the turn it names included — must reach the \
             user intact: {}",
            notices[0]
        );
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
