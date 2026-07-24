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

use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{
    ConfigGetParams, ConfigSetParams, ConfigUpdate, CostQueryParams, ModelListParams,
    ModelSetParams, ModelStatusParams, PrivacyBoundaryConfig, PromptBlock, PromptTurnParams,
    ProviderConfig, RoutingRule, SessionCreateParams,
};
use teton_protocol::{Phase, PrivacyMode, ProviderId, ProviderKind, SessionMode};

mod client;
mod cost_ui;
mod firstrun;
mod keychain;
mod model_ui;
mod prompt;
mod render;
mod session_ui;

use client::{Connection, UiContext};
use keychain::Keychain;
use prompt::{Prompter, StdinPrompter};
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
    /// machine's RAM floor (BR-3).
    #[arg(long, short = 'y', global = true)]
    yes: bool,

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
        None => run_session(&paths, cli.yes),
        Some(Command::Doctor) => run_doctor(&paths),
        Some(Command::Cost) => run_cost(&paths),
        Some(Command::Model { action }) => match action {
            ModelAction::List => run_model_list(&paths),
            ModelAction::Set { name } => run_model_set(&paths, &name, cli.yes),
            ModelAction::Status => run_model_status(&paths),
        },
        Some(Command::Provider { action }) => match action {
            ProviderAction::Add { id, kind, endpoint } => {
                run_provider_add(&paths, &id, kind.into(), endpoint)
            }
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
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("teton: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The default experience: an interactive freeform session (AC-1).
///
/// This is the client that owns the first-run model prompt: it answers permission
/// requests and model proposals, and `auto_accept` (`--yes`) makes the latter
/// unattended (BR-5).
fn run_session(paths: &DaemonPaths, auto_accept: bool) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut conn = client::ensure_connected(paths, &mut surface)?;

    {
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: auto_accept,
        };

        // A proposal raised before this client attached is never replayed as an
        // event (REQ-547 TASK-004), so look for one before doing anything else —
        // otherwise the local tier would stay gated with no visible reason.
        conn.answer_outstanding_model_proposal(&mut ctx)?;

        let created = conn.call(
            SessionCreateParams {
                mode: SessionMode::Freeform,
                phase: None,
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
            &format!("session {session_id} ready (freeform). Type a prompt; Ctrl-D to end."),
        );

        while let Some(input) = ctx.prompter.ask("› ") {
            let text = input.trim();
            if text.is_empty() {
                continue;
            }
            let params = PromptTurnParams {
                session_id: session_id.clone(),
                prompt: vec![PromptBlock::Text {
                    text: text.to_owned(),
                }],
            };
            match conn.call(params, &mut ctx)? {
                Ok(res) => ctx.surface.line(
                    LineKind::Info,
                    &format!("turn ended ({:?}).", res.stop_reason),
                ),
                Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
                    ctx.surface.line(
                        LineKind::Notice,
                        "this daemon build does not execute prompt turns yet (turn-loop wiring \
                         lands with TASK-013); session and event rendering are ready.",
                    );
                    break;
                }
                Err(err) => ctx
                    .surface
                    .line(LineKind::Error, &format!("prompt failed: {}", err.message)),
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
    query_and_render_cost(&mut conn, &mut surface, &mut state, &mut prompter)?;
    let _ = surface.flush();
    Ok(())
}

/// Query the daemon's authoritative cost report (`cost/query`, BR-2 / AC-4) and
/// render it, or print a graceful notice when the daemon does not expose the
/// method or cannot answer. Every figure — totals, baseline, savings — comes from
/// the daemon; the CLI computes none of it (REQ-544 M-7).
fn query_and_render_cost(
    conn: &mut Connection,
    surface: &mut dyn Surface,
    state: &mut SessionState,
    prompter: &mut dyn Prompter,
) -> anyhow::Result<()> {
    let result = {
        let mut ctx = passive_ctx(&mut *surface, &mut *state, &mut *prompter);
        conn.call(CostQueryParams::default(), &mut ctx)?
    };
    match result {
        Ok(res) => cost_ui::render_report_view(&res.report, surface),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => surface.line(
            LineKind::Notice,
            "this daemon build does not expose the cost/query method yet; no authoritative \
             cost report is available.",
        ),
        Err(err) => surface.line(
            LineKind::Error,
            &format!("cost query failed: {}", err.message),
        ),
    }
    Ok(())
}

/// A context for a one-shot command: it renders the daemon's broadcasts but
/// answers nothing. Permission requests and model proposals belong to whichever
/// interactive session owns them — a `teton cost` running in another terminal
/// must not silently answer a prompt the user is looking at elsewhere.
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
/// The BR-3 second confirmation is applied here too, and for the same reason it
/// exists in the first-run prompt: an above-RAM-floor pick is the user's call but
/// must never happen by accident. The fit comes from `model/list` (the daemon
/// computes it), and the daemon independently refuses the change unless
/// `confirmed_above_ram_floor` is set — this is the legible half of that guard,
/// not the guard itself.
fn run_model_set(paths: &DaemonPaths, name: &str, assume_yes: bool) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);

    let list = match conn.call(ModelListParams::default(), &mut ctx)? {
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

    let Some(model) = list.models.iter().find(|m| m.entry.name == name) else {
        let names: Vec<&str> = list.models.iter().map(|m| m.entry.name.as_str()).collect();
        ctx.surface.line(
            LineKind::Error,
            &format!(
                "no catalog entry named `{name}`. Available: {}",
                names.join(", ")
            ),
        );
        return Ok(());
    };

    // BR-3: above this machine's RAM floor needs an explicit second answer.
    let above_floor = model.entry.ram_floor_bytes > list.probe.total_ram_bytes;
    if above_floor && !assume_yes {
        let confirmed = model_ui::confirm_above_ram_floor(
            name,
            model.entry.ram_floor_bytes,
            list.probe.total_ram_bytes,
            &mut *ctx.surface,
            &mut *ctx.prompter,
        );
        if !confirmed {
            ctx.surface.line(
                LineKind::Notice,
                &format!("selection unchanged; `{name}` was not sent."),
            );
            return Ok(());
        }
    } else if above_floor {
        ctx.surface.line(
            LineKind::Notice,
            &format!(
                "`{name}` needs more RAM than this machine has; --yes supplies the second \
                 confirmation (BR-3)."
            ),
        );
    }

    let params = ModelSetParams {
        name: name.to_owned(),
        confirmed_above_ram_floor: above_floor,
    };
    match conn.call(params, &mut ctx)? {
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
    match conn.call(ModelStatusParams::default(), &mut ctx)? {
        Ok(status) => {
            let base_dir = paths.socket.parent();
            let path = match (base_dir, status.install.as_ref()) {
                (Some(base), Some(install)) => {
                    Some(model_ui::weights_path(base, &install.model_name))
                }
                _ => None,
            };
            model_ui::render_status(&status, path.as_deref(), ctx.surface);
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not expose model/status yet.",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read the model status: {}", err.message),
        ),
    }
    Ok(())
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
            "daemon: not running (run `teton` to autostart it, or start `tetond`).",
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

/// `teton cost`: render the daemon's authoritative persisted cost report (AC-4,
/// BR-2). Sources every figure from the daemon's `cost/query` RPC — no live-event
/// draining, no client-side repricing (REQ-544 M-7).
fn run_cost(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    query_and_render_cost(&mut conn, &mut surface, &mut state, &mut prompter)?;
    let _ = surface.flush();
    Ok(())
}

/// `teton provider add`: store the key in the keychain (BR-7), then register.
fn run_provider_add(
    paths: &DaemonPaths,
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let keychain = keychain::default_keychain();
    // Local providers have no credential; every remote kind requires a key.
    let secret = if matches!(kind, ProviderKind::Local) {
        None
    } else {
        Some(read_secret(id)?)
    };
    let config =
        build_provider_registration(id, kind, endpoint, keychain.as_ref(), secret.as_deref())?;
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
        surface.line(
            LineKind::Info,
            &format!(
                "  {} [{}]  {endpoint}  auth: {auth}",
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
    fn provider_add_parses_kind_and_endpoint() {
        let cli = parse(&[
            "teton",
            "provider",
            "add",
            "deepseek",
            "--kind",
            "openai-compatible",
            "--endpoint",
            "https://api.deepseek.com",
        ]);
        match cli.command {
            Some(Command::Provider {
                action: ProviderAction::Add { id, kind, endpoint },
            }) => {
                assert_eq!(id, "deepseek");
                assert!(matches!(kind, CliProviderKind::OpenaiCompatible));
                assert_eq!(endpoint.as_deref(), Some("https://api.deepseek.com"));
                assert_eq!(ProviderKind::from(kind), ProviderKind::OpenaiCompatible);
            }
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
            build_provider_registration("local", ProviderKind::Local, None, &keychain, None)
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
}
