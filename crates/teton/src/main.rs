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

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

// REQ-578: the one home of the per-kind canonical request paths. Aliased on the
// way in because this binary's `ProviderKind` is the *wire* one it parses and
// sends; the composition rule is written against the core enum, and [`core_kind`]
// is the only place the two meet.
use teton_core::session_root::{resolve_cwd_argument, CwdArgError};
use teton_core::{
    canonical_request_path, compose_endpoint, is_absolute_http_url, is_cleartext_to_a_remote_host,
    url_host, ProviderKind as CoreProviderKind,
};
use teton_protocol::effort::EffortLevel;
use teton_protocol::handshake::HandshakeResult;
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    CategoryBindingConfig, ConfigGetParams, ConfigSetParams, ConfigSetResult, ConfigSnapshot,
    ConfigUpdate, ContentClass, CostQueryParams, CostQueryResult, CostReportView, ModelListParams,
    ModelListResult, ModelSetParams, ModelStatusParams, ModelStatusResult, PrivacyBoundaryConfig,
    ProviderConfig, SessionCreateParams, SessionPermissionsParams, TierBindingConfig,
};
use teton_protocol::SessionId;
use teton_protocol::{
    BindingSource, Category, ConfigurableCategory, PrivacyMode, ProviderId, ProviderKind,
    SessionMode, Tier, TierBindingSource,
};

mod banner;
mod cli_rows;
mod client;
mod cost_ui;
mod effort_ui;
mod firstrun;
mod keychain;
mod loading;
mod model_ui;
mod prompt;
mod provider_setup_ui;
mod provider_test_ui;
mod render;
mod service;
mod session_ui;
mod slash;
mod status;
mod uninstall;
mod web_setup_ui;

use client::{Connection, UiContext};
use keychain::{Cleanup, Keychain, PriorKey};
use prompt::{FramedStdinPrompter, Prompter, StdinPrompter};
use render::{stdout_surface, stdout_surface_with_color, LineKind, Surface};
use session_ui::SessionState;
use teton_protocol::socket_path::{self, DaemonPaths};

/// The `teton` command-line interface.
///
/// `pub(crate)` since REQ-582 (BR-3, ADR-2): the session parses a mirrored row's
/// arguments with **this** definition — `Cli::try_parse_from` over the argv its
/// shell twin would have received — so `/policy set-tier build kimi --fallback
/// local` and `teton policy set-tier build kimi --fallback local` are one
/// grammar, one error message, one help text. A second hand-written parser of
/// `teton …` lines is the shape LESSON-529 is about.
#[derive(Debug, Parser)]
#[command(
    name = "teton",
    version,
    about = "Teton Code — hybrid local/remote AI coding agent with workflow-aware routing",
    long_about = None,
)]
pub(crate) struct Cli {
    /// Answer the first-run local-model prompt with "accept" and read no input
    /// (REQ-547 BR-5): the explicit opt-in for unattended/CI runs. Also supplies
    /// the second confirmation `teton model set` needs for a model above this
    /// machine's RAM floor (BR-3), the same confirmation for the in-session
    /// `/model set <name>` (REQ-555 BR-4b — one flow, so the session inherits
    /// the flag as the explicit unattended stand-in and consumes no input line
    /// for the question), the register-this? confirmation the in-session
    /// `/provider add` asks before it reads a key (REQ-582), and the deletion
    /// confirmation of `teton uninstall`.
    ///
    /// And it answers the send-this? question of `/provider test <id>` /
    /// `teton provider test <id>` (REQ-581 BR-2) — the first thing this flag
    /// authorises that puts bytes on the network and money on a user's account.
    /// Everything above it changes this machine; that one calls a vendor. It is
    /// still the same consent, given in advance instead of at a prompt, which is
    /// what a pipe has instead of a terminal.
    #[arg(long, short = 'y', global = true)]
    pub(crate) yes: bool,

    /// Show routing and turn-end notices in the interactive session. By default
    /// the transcript is just the conversation — model responses and tool
    /// activity; privacy and degradation warnings always show.
    #[arg(long, short = 'v', global = true)]
    pub(crate) verbose: bool,

    /// Session root for this session — the directory tools are scoped to —
    /// instead of the shell's directory. A relative path resolves against the
    /// shell's directory and `~` expands (REQ-583 BR-6); the daemon validates it
    /// exactly as it validates the shell's directory today.
    ///
    /// Deliberately **not** `global`: a global flag is stepped over by the
    /// session's `teton …` line classifier and silently dropped by every
    /// mirrored row (`LEADING_GLOBAL_FLAGS`, `cli_rows::run_mirrored_seamed`), and
    /// a session root is a fact about how a session *starts*, not something a
    /// row inside one should ever parse.
    #[arg(long, value_name = "PATH")]
    pub(crate) cwd: Option<String>,

    /// The subcommand to run; omit to open an interactive session.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
    /// Show or set the global reasoning-effort level (REQ-559).
    ///
    /// With no argument, prints the current level and what it resolves to for
    /// each registered provider. With one, sets it — and the setting persists
    /// across sessions (BR-8).
    Effort {
        /// One of: low, medium, high, xhigh, max. Omit to read the current
        /// setting.
        level: Option<String>,
    },
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
pub(crate) enum ModelAction {
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
pub(crate) enum ProviderAction {
    /// Register a provider; its key is stored in the OS keychain (BR-7).
    Add {
        /// Provider id (e.g. `anthropic`, `deepseek`).
        id: String,
        /// Provider family.
        #[arg(long, value_enum)]
        kind: CliProviderKind,
        /// Endpoint URL: your vendor's documented base URL or the full request
        /// URL. For `openai-compatible` and `anthropic` a base URL is completed
        /// to the request URL and echoed; `custom` is stored as typed. Required
        /// for remote kinds except `anthropic`, which defaults to the official
        /// Messages URL.
        #[arg(long)]
        endpoint: Option<String>,
        /// The model this provider calls, e.g. `claude-opus-5` (REQ-557 BR-1).
        /// Required for remote kinds; never inferred from the provider id.
        #[arg(long)]
        model: Option<String>,
    },
    /// List configured providers.
    List,
    /// Test a registered provider by making one minimal, consented call to it
    /// (REQ-581 BR-7).
    ///
    /// It asks before it sends; `--yes` consents up front, which is what a
    /// script or a piped shell needs. Unlike every other `teton provider`
    /// subcommand this one opens a session, because it is not a read: the method
    /// is session-gated so a tool-spawned copy cannot spend the user's provider
    /// budget, and the ledger row it writes needs a session to belong to
    /// (architecture ADR-5).
    Test {
        /// Provider id, as `teton provider list` names it.
        id: String,
    },
}

/// `teton boundary …`
#[derive(Debug, Subcommand)]
pub(crate) enum BoundaryAction {
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
pub(crate) enum PolicyAction {
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
pub(crate) enum CliProviderKind {
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
pub(crate) enum CliPrivacyMode {
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
pub(crate) enum CliTier {
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
pub(crate) struct CliCategory(pub(crate) ConfigurableCategory);

/// Parse a category argument, naming the pin when a user types a pinned one.
///
/// Deliberately **not** a `ValueEnum`: clap would reject `redact` with
/// "invalid value 'redact' … [possible values: title, digest, …]", which reads
/// like a typo. AC-4's criterion is that a user who names a pinned category
/// learns it is *forbidden*, and that sentence comes from the protocol's
/// `FromStr` rather than being written a third time here.
pub(crate) fn parse_cli_category(name: &str) -> Result<CliCategory, String> {
    name.parse::<ConfigurableCategory>()
        .map(CliCategory)
        .map_err(|e| e.to_string())
}

/// The user's home folder, from `HOME` — `None` when unset or empty, never a
/// guess. The one read of the variable in this binary: the banner's `cwd:`
/// spelling, `--cwd`'s `~` and `/cd`'s `~` all take their home from here.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
}

/// The session root this process asks the daemon for (REQ-583 BR-6): the
/// resolved `--cwd`, or `shell_cwd` — today's behaviour, `None` when the shell's
/// directory is unreadable — when the flag is absent.
///
/// Pure, so the `--cwd` parse test can drive it with teton-core's grammar table
/// (AC-12): the flag's value goes through [`resolve_cwd_argument`], the same
/// function `/cd` uses, and nothing else — no canonicalization and no
/// existence check, because the daemon validates the path it is sent (ADR-4).
///
/// # Errors
/// [`CwdArgError`] when `--cwd` was given and could not become an absolute
/// path (empty, `~` without a home, or a relative path with no shell directory
/// to join it onto).
pub(crate) fn session_root_for(
    cwd_flag: Option<&str>,
    shell_cwd: Option<&Path>,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, CwdArgError> {
    match cwd_flag {
        None => Ok(shell_cwd.map(Path::to_path_buf)),
        Some(raw) => {
            // With no shell directory a relative argument cannot join onto
            // anything, and `resolve_cwd_argument` says so (`NotAbsolute`); an
            // absolute or `~` argument still resolves.
            let shell_cwd = shell_cwd.unwrap_or_else(|| Path::new(""));
            resolve_cwd_argument(raw, shell_cwd, home).map(Some)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = socket_path::daemon_paths();

    // REQ-583 BR-6: the session root is resolved once, here at the edge —
    // `--cwd` through the grammar `/cd` shares, or the shell's directory as
    // before — and threaded to the two places a session is created. A malformed
    // `--cwd` is refused before anything connects, in the `bail!` shape every
    // other refused argument takes: one line on stderr, non-zero exit.
    let session_root = match session_root_for(
        cli.cwd.as_deref(),
        std::env::current_dir().ok().as_deref(),
        home_dir().as_deref(),
    ) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("teton: --cwd: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = match cli.command {
        None => run_session(&paths, cli.yes, cli.verbose, session_root.as_deref()),
        Some(Command::Doctor) => run_doctor(&paths),
        Some(Command::Cost) => run_cost(&paths),
        Some(Command::Effort { level }) => run_effort(&paths, level.as_deref()),
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
            ProviderAction::Test { id } => {
                run_provider_test(&paths, &id, cli.yes, session_root.as_deref())
            }
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

/// The one line [`run_mirrored_command`] renders for a `Command` that is not one
/// of the ten mirrored rows (REQ-582 verify, m3/T12).
///
/// **Unreachable by construction.** The dispatcher's only caller is
/// [`cli_rows::run_mirrored`], whose argv always opens with a mirrored row's own
/// twin (`teton policy set-tier …`), so the parsed `Command` is always one of
/// the ten. `teton cost`, `teton effort`, `teton model set`, `teton provider
/// test` typed at the prompt run their own session rows through
/// [`slash::run_cli_line`] without passing through here; `teton uninstall` and
/// the retired `teton policy set` are refused by the classifier before any row
/// runs ([`cli_rows::refusal_for_path`]). What is left is the six arms the
/// exhaustive match still has to name, and one sentence is enough for all of
/// them: it says nothing was run and points at `/help`, which lists what can be.
/// A per-arm sentence here would be a second, unreachable copy of what the
/// classifier already says — and the arms exist for the compiler, not the user.
fn not_a_mirrored_row(spelling: &str) -> String {
    format!(
        "`teton {spelling}` is not one of the commands this session runs through its shell twin, \
         so nothing was run — {}",
        slash::HELP_HINT
    )
}

/// Run a parsed `Command` over a connection and context the caller already has
/// (REQ-582 ADR-2 step 3).
///
/// The session's half of [`main`]'s match: the same `Command` tree, dispatched
/// onto the same `*_on` bodies the subcommands run, so a mirrored row cannot
/// send different params or render through a different function than its shell
/// twin (BR-2/BR-3).
///
/// The match is **exhaustive with no wildcard**, which is the compile-time half
/// of ADR-8's completeness property: a subcommand added later cannot ship
/// without a decision about its session form. A variant that is not a mirrored
/// row renders exactly one [`LineKind::Error`] line ([`not_a_mirrored_row`])
/// and returns `Ok` — never a panic, and never an `Err`, which would end the
/// session that asked. Those arms are unreachable from every caller that exists;
/// the test that drives them calls this function directly with hand-built
/// commands.
///
/// `DaemonPaths` is re-read here rather than threaded through [`UiContext`]: it
/// is a pure environment read that `run_session` already made, and a copy on the
/// context would be a second source of truth for the socket's location (ADR-5).
///
/// # Errors
///
/// Propagates a transport error from whichever body ran; a daemon that answers
/// is reported on the surface. [`cli_rows::run_mirrored`], its one caller,
/// propagates it in turn, so a lost socket ends the session the way it ends
/// `/cost`'s (REQ-582 verify, m7).
pub(crate) fn run_mirrored_command(
    cmd: Command,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    match cmd {
        Command::Provider { action } => match action {
            ProviderAction::List => provider_list_on(conn, ctx),
            ProviderAction::Add {
                id,
                kind,
                endpoint,
                model,
            } => {
                // ADR-3: the refusals are outcomes here, not `bail!`s — one
                // Error line, and the session carries on. The consent mode is
                // the session's: a default-no confirmation before the key is
                // read, pre-answered by the session's own `--yes` exactly as
                // `/model set`'s second confirmation is (verify M1). The
                // keychain is the platform's, passed rather than built inside
                // so the composed flow is drivable against a double (M4).
                let consent = AddConsent::Session {
                    assume_yes: ctx.auto_accept_model,
                };
                let keychain = keychain::default_keychain();
                if let Err(refusal) = provider_add_on(
                    conn,
                    ctx,
                    &id,
                    kind.into(),
                    endpoint,
                    model,
                    consent,
                    keychain.as_ref(),
                )? {
                    ctx.surface.line(LineKind::Error, &refusal.to_string());
                }
                Ok(())
            }
            ProviderAction::Test { .. } => {
                ctx.surface
                    .line(LineKind::Error, &not_a_mirrored_row("provider test"));
                Ok(())
            }
        },
        Command::Boundary { action } => match action {
            BoundaryAction::List => boundary_list_on(conn, ctx),
            BoundaryAction::Add { glob, mode } => boundary_add_on(conn, ctx, &glob, mode.into()),
        },
        Command::Policy { action } => match action {
            PolicyAction::Show => policy_show_on(conn, ctx),
            PolicyAction::SetTier {
                tier,
                provider,
                fallback,
            } => policy_set_tier_on(conn, ctx, tier.into(), &provider, fallback.as_deref()),
            PolicyAction::SetCategory {
                category,
                provider,
                fallback,
            } => policy_set_category_on(conn, ctx, category.0, &provider, fallback.as_deref()),
            PolicyAction::Set { .. } => {
                ctx.surface
                    .line(LineKind::Error, &not_a_mirrored_row("policy set"));
                Ok(())
            }
        },
        Command::Model { action } => match action {
            ModelAction::List => model_list_on(conn, ctx),
            ModelAction::Status => model_status_on(&socket_path::daemon_paths(), conn, ctx),
            ModelAction::Set { .. } => {
                ctx.surface
                    .line(LineKind::Error, &not_a_mirrored_row("model set"));
                Ok(())
            }
        },
        Command::Doctor => {
            // BR-7: the facts are read off the connection this session already
            // has; dialling the socket again would announce an attach into the
            // very session being diagnosed (BUG-177's shape).
            let attach = DoctorAttach::session(conn);
            doctor_report_on(&socket_path::daemon_paths(), conn, ctx, &attach)
        }
        Command::Cost => {
            ctx.surface
                .line(LineKind::Error, &not_a_mirrored_row("cost"));
            Ok(())
        }
        Command::Effort { .. } => {
            ctx.surface
                .line(LineKind::Error, &not_a_mirrored_row("effort"));
            Ok(())
        }
        Command::Uninstall { .. } => {
            ctx.surface
                .line(LineKind::Error, &not_a_mirrored_row("uninstall"));
            Ok(())
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
    entry.set_status(entry_status(ctx));
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
            // Recomposed on every redraw rather than cached: the level can have
            // changed since the last draw, and a stale row about permissions is
            // worse than none.
            entry.set_status(entry_status(ctx));
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

/// The status row's content for the next frame draw, or `None` for no row
/// (REQ-560).
///
/// Thin on purpose: what the row *says* is [`status::status_line`]'s decision,
/// unit-tested with no terminal in the way (BR-8), and the only thing added here
/// is the two runtime facts it cannot be pure about — the session's level and
/// the terminal's width.
///
/// `None` propagates from two places and means the same thing in both: a level
/// nobody has read yet, and a terminal too narrow for the row. Neither is an
/// error — the values stay readable through bare `/permissions`, which works on
/// a pipe (BR-10).
///
/// The effort field is `None` until REQ-559 lands; this REQ renders the
/// permission level alone and adds no `/effort` command (BR-14).
fn entry_status(ctx: &UiContext<'_>) -> Option<String> {
    let level = ctx.state.permission_level?;
    let effort = status::effort_field(ctx.state.effort.as_ref());
    status::status_line(level, effort.as_deref(), prompt::terminal_width())
}

/// Read the session's permission level into the render cache (REQ-560).
///
/// Best-effort by design. A daemon that does not serve `session/permissions`
/// leaves the level `None`, which draws no status row — the session is fully
/// usable and the level is still reachable with `/permissions`, so a failure
/// here costs a row and nothing else (BR-13). The failure is deliberately
/// **silent**: an error line at every startup against an older daemon would be
/// noise about a feature the user has not asked for yet.
fn read_permission_level(conn: &mut Connection, ctx: &mut UiContext<'_>, session_id: &SessionId) {
    let params = SessionPermissionsParams {
        session_id: session_id.clone(),
        level: None,
    };
    if let Ok(Ok(result)) = conn.call(params, ctx) {
        ctx.state.permission_level = Some(result.level);
    }
}

/// Read the daemon's config view into the render cache: the reasoning-effort
/// view (REQ-559 / REQ-560) and the web capability state (REQ-572).
///
/// One `config/get`, two fields, because they come off one snapshot and a second
/// call would be a second round-trip for a row that draws once. Best-effort for
/// the same reason [`read_permission_level`] is: a daemon that predates either
/// field leaves it `None`, and the status row then shows what it can. Silent,
/// because an error line at every startup against an older daemon would be noise
/// about a feature the user has not asked for.
///
/// Both halves are kept fresh by whoever changes them: `/effort`'s handler
/// caches what the daemon reports after a set, and the `web_setup_completed`
/// event folds the new ceiling in — so neither can go on showing a value the
/// user has already changed.
fn read_config_view(conn: &mut Connection, ctx: &mut UiContext<'_>) {
    if let Ok(Ok(cfg)) = conn.call(ConfigGetParams::default(), ctx) {
        ctx.state.effort = cfg.snapshot.effort;
        // The daemon's own derivation, from the predicate that also governs tool
        // exposure — never a second reading of `[web] tier` here (REQ-572 BR-3).
        ctx.state.web.capability = cfg.snapshot.web_capability;
        // The registered ids, for REQ-581 ADR-4's connection-question predicate:
        // "is kimi working?" is a provider question because `kimi` is one of
        // *this* user's providers, which is a fact only the snapshot has. A
        // daemon that answers with none leaves the list empty, which the
        // predicate's fixed subject words already cover.
        ctx.state.provider_ids = cfg
            .snapshot
            .providers
            .iter()
            .map(|provider| provider.id.0.clone())
            .collect();
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
///
/// Since REQ-580 the daemon **holds** such a turn rather than refusing it —
/// the client sees a `turn_queued` notice ([`session_ui`]'s renderer) and then
/// the reply — so this headline reaches a session only from the paths the hold
/// does not cover: a fallback that landed on the warming tier after a remote
/// primary failed, or a daemon older than the hold. It stays for both.
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

/// The one notice a recognized `teton …` line prints before its row runs
/// (REQ-582 BR-4 / AC-5).
///
/// `teton provider list → /provider list`: the canonical `teton` spelling of the
/// row that ran, and the `/` spelling this session uses for it. "Canonical"
/// rather than "as typed" — the row name is what clap's walk resolved the line
/// to, so extra whitespace, a leading global flag and (were one ever declared)
/// a subcommand alias all render as the one spelling `/help` lists (verify
/// m11). It is a [`LineKind::Notice`] because that is the class for "a control
/// decision was made and here it is" — the surface draws it as `>> …` — and it
/// is one line because the answer the user actually asked for follows it
/// immediately.
///
/// Both halves come from the row name, which *is* the subcommand path clap
/// walked to (ADR-1), so the line cannot name a `/` spelling that does not
/// dispatch.
fn cli_line_note(name: &str) -> String {
    format!("teton {name} → /{name}")
}

/// The default experience: an interactive freeform session (AC-1).
///
/// This is the client that owns the first-run model prompt: it answers permission
/// requests and model proposals, and `auto_accept` (`--yes`) makes the latter
/// unattended (BR-5).
///
/// `session_root` is the directory the session's tools are scoped to (REQ-583
/// BR-6): the resolved `--cwd`, or the shell's directory. It is what the banner's
/// `cwd:` line shows and what `session/create` is sent; `None` only when the
/// shell's directory was unreadable and no `--cwd` was given, in which case the
/// daemon falls back to its own root as it always has.
///
/// # Errors
///
/// A transport error, or a `session/create` the daemon refused — the refusal
/// names the path and the reason, and it is an error exit (BR-6: never a session
/// that starts and then fails on every tool; a script must see the failure).
fn run_session(
    paths: &DaemonPaths,
    auto_accept: bool,
    verbose: bool,
    session_root: Option<&Path>,
) -> anyhow::Result<()> {
    // The banner is for humans at a terminal. Piped stdout (the e2e suites,
    // shell composition) sees the same byte stream it always did.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Colour is the surface's to apply, so it is decided before the surface
    // exists — the banner names its line classes and the surface draws them.
    let color = interactive && banner::color_enabled();

    let mut surface = stdout_surface_with_color(color);
    let mut state = SessionState::new();
    state.verbose = verbose;
    // The same terminal fact, carried on the state for the one arm that draws
    // TTY-only bytes without reaching this scope (REQ-583 BR-8's re-fire of
    // the not-a-project notice; `session_ui::SessionState::interactive`).
    state.interactive = interactive;
    let mut prompter = StdinPrompter::new();

    // The *other* half of "interactive", read once here at the edge and carried
    // on the context (REQ-555): where the entry lines come from, which is what
    // the `/model set` gate turns on. Two different questions — a session may
    // well have a piped stdin and a terminal stdout — so neither flag is
    // derivable from the other, and a handler must never read either itself.
    let typed_input = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if interactive {
        // The `cwd:` line shows the *session root* — the resolved `--cwd` when
        // one was given — never the shell's directory when the two differ
        // (BR-6), spelled by the daemon's own display rule (ADR-1).
        banner::print(
            &mut surface,
            env!("CARGO_PKG_VERSION"),
            session_root.map(banner::cwd_display).as_deref(),
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
                // jail must be THIS terminal's directory — or the `--cwd` the
                // user named instead of it (REQ-583 BR-6) — so send it.
                cwd: session_root.map(Path::to_path_buf),
            },
            &mut ctx,
        )?;
        let (session_id, root) = match created {
            Ok(res) => (res.session_id, res.root),
            // BR-6: a refused create is one line naming the path and the reason,
            // and an error exit — `Ok(())` here read as success to a script, and
            // "never a session that starts and then fails" needs the failure to
            // be one. `bail!` at the binary edge, as every refused argument is
            // (REQ-582 ADR-3): `main` prints it once, on stderr, and nothing
            // about a session follows.
            Err(err) => anyhow::bail!("could not start a session: {}", err.message),
        };
        // The slash handlers act on this session and reach it only through the
        // context (REQ-563: `/web allow` names the session whose restriction it
        // lifts), and the renderer needs it to tell this session's events from
        // another session's on the daemon-wide bus (REQ-567 BR-8).
        ctx.session_id = Some(session_id.clone());
        ctx.state.session_id = Some(session_id.clone());
        // The root the daemon settled on (REQ-583): a cache of a daemon fact
        // for `/cd`'s bare form, refreshed by every `session_root_changed`.
        // `None` from a daemon older than the field, and nothing here assumes
        // otherwise.
        ctx.state.root = root;
        if interactive {
            // BR-5: a root that is not a project is announced once, under the
            // banner and before the ready line — the same TTY gate as the
            // banner, so piped output is byte-identical (ADR-5). Content is
            // `banner::root_notice`'s; only the bytes are gated here.
            if let Some(notice) = ctx.state.root.as_ref().and_then(banner::root_notice) {
                ctx.surface.line(LineKind::Notice, &notice);
            }
        }
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
        // Plain text only: the chevron's tint is applied inside the framed
        // prompter, after defusing (REQ-573 — caller-composed SGR in a
        // question is sanitizer food, not styling).
        let entry_prompt = if interactive { " › " } else { "› " };
        let mut entry = FramedStdinPrompter::new(interactive, color);
        // REQ-560: seed the status row's permission field once, from the daemon.
        // Only when interactive — BR-9 draws no row on a pipe, so a piped
        // session must not pay an RPC for something it will never render, and a
        // daemon too old to answer leaves the level `None` and the row absent
        // rather than showing a guess.
        if interactive {
            read_permission_level(&mut conn, &mut ctx, &session_id);
            read_config_view(&mut conn, &mut ctx);
        }
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
                // REQ-582 BR-4/BR-5: a typed `teton …` line whose subcommand
                // path names a row runs **that row**, through the same
                // `dispatch` a `/` line reaches — no `std::process::Command`,
                // no second `Connection`, no prompt turn. The invariant is
                // structural rather than checked: recognition ends in a table
                // lookup, so it can only run what the table lists, on the
                // connection this session already holds (D-4). Spawning the
                // binary instead would announce an attach into the very session
                // that typed the line (BUG-177's shape).
                slash::Input::CliLine {
                    name,
                    args,
                    shell_flags,
                } => {
                    // First, and always: the line the user typed is not the
                    // spelling this session uses, and one notice is how they
                    // learn the one that is (AC-5). Then the row, through
                    // `run_cli_line` rather than `dispatch` directly: a row
                    // that predates this REQ (`/model set`, `/effort`, …) has
                    // its whole typed argv validated by the binary's own parser
                    // first, so `teton model set qwen --yes` cannot hand the
                    // row "qwen --yes" as a model name (verify M2).
                    ctx.surface.line(LineKind::Notice, &cli_line_note(name));
                    match slash::run_cli_line(name, args, shell_flags, &mut conn, &mut ctx)? {
                        slash::CommandOutcome::Continue => continue,
                        slash::CommandOutcome::Quit => break,
                    }
                }
                // A real command with no session form: one line saying why and
                // where to go instead, composed by the classifier from the same
                // clap tree that recognized the path (BR-4). No RPC, no turn.
                slash::Input::CliRefused(refusal) => {
                    ctx.surface.line(LineKind::Error, &refusal);
                    continue;
                }
                // `teton provider --help`: the parser's own help page for that
                // family, rendered as information — a user who asked for help
                // got what they asked for, and no line of it is an error
                // (verify T6). No RPC, no turn.
                slash::Input::CliHelp(text) => {
                    cli_rows::render_clap_text(&text, false, &mut *ctx.surface);
                    continue;
                }
                // The escape hatch has already collapsed its leading pair
                // (BR-1b); a plain prompt is the trimmed line's own bytes.
                slash::Input::EscapedPrompt(text) | slash::Input::Prompt(text) => text,
            };
            // Built by the classifier's own module, so the bytes on the wire are
            // the bytes it classified (AC-7 / AC-7b) rather than a second
            // reading of the line taken here.
            let params = slash::prompt_turn_params(&session_id, prompt_text);
            // REQ-579 ADR-9: the hand-off check reads *this* turn's reply, so
            // the accumulator opens here — at the send — rather than closing at
            // the previous turn's end. A turn that was interrupted or refused
            // never reached `hand_off_after_turn`, and without this its words
            // would still be sitting there when the next reply arrived.
            //
            // REQ-581 ADR-4 opens it with the **question** as well: the same
            // bytes `prompt_turn_params` just put on the wire, so what the
            // connection predicate reads is what was asked rather than a second
            // reading of the input line taken here.
            ctx.state.begin_turn(prompt_text);
            match conn.call(params, &mut ctx)? {
                Ok(res) => {
                    // REQ-579 ADR-9. Before the turn's closing line, because
                    // the hand-off is about the reply that just finished and
                    // belongs beside it rather than after the gap that ends the
                    // turn. Gated on `typed_input`: a piped session already got
                    // the shell recipe from BR-11 and its bytes must not move.
                    session_ui::hand_off_after_turn(ctx.state, ctx.surface, ctx.typed_input);
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
///
/// A connection-opening shell around [`model_list_on`], which is also what
/// `/model list` runs (REQ-582 BR-2, ADR-3).
fn run_model_list(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    model_list_on(&mut conn, &mut ctx)
}

/// The body of `model list`: one `model/list`, one renderer (REQ-582 BR-2).
///
/// Takes the caller's connection and context and creates neither, so the
/// subcommand runs it under its passive context while `/model list` runs it
/// under the session's own (REQ-555 D-4) — one call site of `model/list` and
/// one call site of [`model_ui::render_list`], which is what keeps the two
/// surfaces from drifting.
///
/// # Errors
///
/// Propagates a transport error. A daemon that *answers* — with an error, or
/// with "no such method" — is reported on the surface and returns `Ok`.
pub(crate) fn model_list_on(conn: &mut Connection, ctx: &mut UiContext<'_>) -> anyhow::Result<()> {
    match conn.call(ModelListParams::default(), ctx)? {
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
    model_status_on(paths, &mut conn, &mut ctx)
}

/// The body of `model status`: one `model/status`, one renderer (REQ-582 BR-2).
///
/// `paths` rather than a connection-derived value because the weights directory
/// is a fact about *this machine's* daemon state directory, which no payload
/// carries (BR-11 keeps absolute paths off the wire). It is a pure env read
/// ([`socket_path::daemon_paths`]) that every caller already has or can make,
/// so it is passed rather than threaded through [`UiContext`] (REQ-582 ADR-5).
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn model_status_on(
    paths: &DaemonPaths,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    let answered = conn.call(ModelStatusParams::default(), ctx)?;
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

/// Which connection the doctor report is describing — the **only** thing that
/// differs between `teton doctor` and the in-session `/doctor` (REQ-582 BR-7,
/// ADR-5).
///
/// A session must not dial the socket afresh to diagnose itself: a second
/// `Connection::connect` announces `a CLI client attached` into the very session
/// running the diagnosis, and calls it "another" client (BUG-177's shape). So
/// the connect/handshake arm becomes this one line, and every other line of the
/// report — the header, the config listing, the base-URL advice, the model and
/// provider notices — is the same code on both paths.
pub(crate) enum DoctorAttach {
    /// The shell path: a handshake this command performed itself, which is the
    /// only place the negotiated protocol version is known.
    Fresh(HandshakeResult),
    /// The session path: the facts the session's already-open connection kept
    /// from its own handshake. Owned rather than borrowed from the `Connection`
    /// because [`doctor_report_on`] takes that connection mutably to make the
    /// `config/get` call.
    Session {
        /// `conn.daemon_name()` at the time the attach was made.
        daemon_name: Option<String>,
        /// `conn.daemon_version()` at the time the attach was made.
        daemon_version: Option<String>,
    },
}

impl DoctorAttach {
    /// The attach describing a session's own connection (BR-7).
    ///
    /// Reads the handshake facts the [`Connection`] already keeps for build-skew
    /// reporting (REQ-565 BR-6) — no second handshake, no second RPC.
    pub(crate) fn session(conn: &Connection) -> Self {
        Self::Session {
            daemon_name: conn.daemon_name().map(str::to_owned),
            daemon_version: conn.daemon_version().map(str::to_owned),
        }
    }

    /// The one `daemon: running — …` line the two arms disagree about.
    ///
    /// The fallbacks are unreachable in practice: a `Session` attach is made
    /// from a connection that has completed a handshake by construction (every
    /// command reaches the daemon through `ensure_connected`, which handshakes).
    /// They exist so the report still renders every other line rather than
    /// panicking on a state that would mean the connection was never negotiated.
    fn daemon_line(&self) -> String {
        match self {
            Self::Fresh(hs) => format!(
                "daemon: running — {} {} (protocol {})",
                hs.daemon_name, hs.daemon_version, hs.protocol_version
            ),
            Self::Session {
                daemon_name,
                daemon_version,
            } => format!(
                "daemon: running — {} {} (this session's connection)",
                daemon_name.as_deref().unwrap_or("teton-code"),
                daemon_version.as_deref().unwrap_or("(version unreported)"),
            ),
        }
    }
}

/// The lines doctor prints before it knows anything about the daemon: what this
/// command is, and where the daemon's socket and lock live.
///
/// Split out because the two arms that cannot produce a report — no daemon, and
/// a daemon that rejected this CLI — still print them, and a second copy of the
/// header would be a second answer to "where is the socket?".
fn doctor_header(paths: &DaemonPaths, surface: &mut dyn Surface) {
    surface.line(LineKind::Info, "teton doctor");
    surface.line(
        LineKind::Info,
        &format!("socket: {}", paths.socket.display()),
    );
    surface.line(LineKind::Info, &format!("lock:   {}", paths.lock.display()));
}

/// Everything doctor prints before it asks the daemon anything: the header and
/// the one line [`DoctorAttach`] decides.
///
/// This is the whole surface the two attach arms can differ on — after it, the
/// report has no `attach` in scope — which is what the unit test over both arms
/// stands on (REQ-582 AC-1's `/doctor` carve-out).
fn doctor_preamble(paths: &DaemonPaths, attach: &DoctorAttach, surface: &mut dyn Surface) {
    doctor_header(paths, surface);
    surface.line(LineKind::Info, &attach.daemon_line());
}

/// The two closing notices, printed on every path including the ones that never
/// reached the daemon.
fn doctor_trailer(surface: &mut dyn Surface) {
    surface.line(
        LineKind::Notice,
        "model: the local-tier lifecycle is event-driven — start a session to observe \
         probe/download/benchmark.",
    );
    // Doctor stays passive (REQ-581 BR-7 / OQ-2): it names the command that
    // sends and does not become it. The clause is here because this is the line
    // a user reads when they came to `doctor` with "is my provider working?" —
    // the question doctor cannot answer, and now the one place that says which
    // command can.
    surface.line(
        LineKind::Notice,
        "providers: reachability is probed by the daemon at call time; the CLI has no network \
         path of its own (BR-1). `teton provider test <id>` makes one consented call and reports \
         what came back.",
    );
}

/// `teton doctor`: daemon status, socket path, model state, providers.
///
/// Keeps the connect/handshake itself — deliberately `Connection::connect` and
/// not `client::ensure_connected`, because a diagnosis must report a daemon
/// that is down rather than start one — and hands the handshake to
/// [`doctor_report_on`] as [`DoctorAttach::Fresh`]. The two arms that have no
/// daemon to report on stay here: a session can be in neither state (REQ-582
/// ADR-5).
fn run_doctor(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();

    match Connection::connect(&paths.socket) {
        Ok(mut conn) => match conn.handshake() {
            Ok(hs) => {
                let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
                doctor_report_on(paths, &mut conn, &mut ctx, &DoctorAttach::Fresh(hs))?;
            }
            // The commonest cause is a daemon left running across an upgrade,
            // and `handshake` has already turned that into a sentence with the
            // restart command in it — doctor adds the context that the daemon
            // is up, which is the part its other arms establish.
            Err(err) => {
                doctor_header(paths, &mut surface);
                surface.line(
                    LineKind::Error,
                    &format!("daemon: reachable, but it rejected this CLI — {err}"),
                );
                doctor_trailer(&mut surface);
            }
        },
        Err(_) => {
            doctor_header(paths, &mut surface);
            surface.line(
                LineKind::Notice,
                "daemon: not running (run `teton` to autostart it, or start `teton-code`).",
            );
            doctor_trailer(&mut surface);
        }
    }
    Ok(())
}

/// The doctor report over a connection the caller already has (REQ-582 BR-7).
///
/// One `config/get` and the same renderers `teton doctor` has always used, so
/// `/doctor` cannot drift from its shell twin: the only line that knows which
/// surface it is on is [`DoctorAttach::daemon_line`].
///
/// # Errors
///
/// Propagates a transport error from `config/get`; a daemon that answers is
/// reported on the surface and returns `Ok`.
pub(crate) fn doctor_report_on(
    paths: &DaemonPaths,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    attach: &DoctorAttach,
) -> anyhow::Result<()> {
    doctor_preamble(paths, attach, ctx.surface);
    match conn.call(ConfigGetParams::default(), ctx)? {
        Ok(cfg) => {
            render_config(&cfg.snapshot.providers, ctx.surface);
            advise_on_base_url_endpoints(&cfg.snapshot.providers, ctx.surface);
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "config: not exposed by this daemon build yet (config/get pending).",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("config query failed: {}", err.message),
        ),
    }
    doctor_trailer(ctx.surface);
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

/// `teton effort [level]` (REQ-559 BR-9).
///
/// The bare form reads; the one-argument form sets and then reads back, so the
/// user sees the clamp their new level lands on rather than only the number they
/// typed. Both render through [`crate::effort_ui::render`] — the same function
/// `/effort` calls — because two surfaces describing one setting must not be
/// able to drift (LESSON-456, REQ-555 BR-4).
fn run_effort(paths: &DaemonPaths, level: Option<&str>) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    if let Some(raw) = level {
        // Parsed client-side so a typo costs one line rather than a round trip,
        // and the error names every accepted spelling from the one list that
        // also defines the enum (`teton_core::effort::level_list`).
        let parsed: EffortLevel = match raw.parse() {
            Ok(l) => l,
            Err(err) => {
                ctx.surface.line(LineKind::Error, &format!("{err}"));
                return Ok(());
            }
        };
        if let Err(err) = conn.call(
            ConfigSetParams {
                update: ConfigUpdate::SetEffort(parsed),
            },
            &mut ctx,
        )? {
            ctx.surface.line(
                LineKind::Error,
                &format!("could not set the effort level: {}", err.message),
            );
            return Ok(());
        }
    }
    // Read back even after a set, so the user sees the clamp their new level
    // lands on for each provider rather than only the number they typed.
    match conn.call(ConfigGetParams::default(), &mut ctx)? {
        Ok(cfg) => effort_ui::render(ctx.surface, cfg.snapshot.effort.as_ref()),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read the effort setting: {}", err.message),
        ),
    }
    Ok(())
}

/// A `provider add` the flow declined to make, and the sentence it declines
/// with (REQ-582 ADR-3).
///
/// The three refusals were `anyhow::bail!`s inside [`run_provider_add`], which
/// is the right channel for a shell (non-zero exit, the message on stderr) and
/// the wrong one for a session: a handler that returns `Err` ends the session,
/// because the entry loop propagates it. So the decision travels as a value and
/// each surface chooses the channel — [`run_provider_add`] maps it straight back
/// to `bail!` with the same sentence, and the session renders it as one
/// [`LineKind::Error`] line and carries on. The **text** is identical either
/// way, which is what AC-1's byte-parity and AC-2/AC-3's effect parity each
/// assert about a different half of this command.
///
/// The verify pass (m7) added the two *failures* that are likewise not the
/// session's to end on: an endpoint the registration seam refuses (REQ-578
/// BR-5) and a keychain that will not store. Both are "this registration did
/// not happen, and here is why" — the same shape as the three decisions above,
/// and the same channel: a shell exits non-zero with the sentence, a session
/// prints it and carries on. Only a **transport** failure stays an `Err` out of
/// [`provider_add_on`], because a lost socket ends every command's session the
/// same way (`/cost`'s included), and a body that swallowed it would report a
/// dead connection one command later than the loop would.
#[derive(Debug)]
pub(crate) enum ProviderAddRefusal {
    /// A remote provider with no `--model` (REQ-557 BR-1).
    RemoteWithoutModel {
        /// The id that was being registered.
        id: String,
    },
    /// An id that is already registered (BUG-155).
    DuplicateId {
        /// The id that already exists.
        id: String,
    },
    /// No credential was supplied — an empty answer, or EOF at the prompt.
    NoKey,
    /// The registration seam refused the endpoint (REQ-578 BR-5): the sentence
    /// [`settle_endpoint_text`] composed, which already ends by saying nothing
    /// was changed and no credential was read.
    Endpoint(String),
    /// The keychain would not store the credential. Nothing was registered and
    /// nothing is left in the keychain — the store is what failed.
    KeychainStore(String),
}

impl std::fmt::Display for ProviderAddRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RemoteWithoutModel { id } => write!(
                f,
                "provider `{id}` is a remote provider and must declare the model it calls: \
                 pass `--model <name>` (e.g. `--model claude-opus-5`). The model is never \
                 inferred from the provider id."
            ),
            Self::DuplicateId { id } => write!(
                f,
                "provider `{id}` is already registered. Ids are unique — pick a different \
                 one (e.g. `{id}-2`) if you want a second provider, which is how one vendor \
                 serves two models. Nothing was changed and no credential was read."
            ),
            Self::NoKey => write!(
                f,
                "no API key provided; set TETON_PROVIDER_KEY or enter the key"
            ),
            // Verbatim: this is the sentence the shell printed for the same
            // refusal before it travelled as a value, and the e2e suite pins
            // fragments of it against the real binary.
            Self::Endpoint(sentence) => f.write_str(sentence),
            Self::KeychainStore(sentence) => f.write_str(sentence),
        }
    }
}

/// Who is running `provider add`, and therefore whether the flow asks before it
/// reads a key (REQ-582 verify, M1).
///
/// A shell command line is its own consent: the user typed `teton provider add
/// … --model …` and pressed return, and the next thing the shell reads is the
/// key. A session line is not — a multi-line paste that opens with `/provider
/// add …` hands its **second line** to the credential prompt, echo-off, and the
/// flow would store whatever it was in the keychain and register a provider
/// against it before the user saw a question. So the session confirms first,
/// default-no, with the settled endpoint on screen ([`AddConsent::Session`]),
/// and a paste answers "no" by not being `y`. The shell's bytes are unchanged
/// (`cli_e2e` is the net for that).
///
/// `assume_yes` is the session's own `--yes` (`ctx.auto_accept_model`), which
/// pre-answers this question exactly as it pre-answers `/model set`'s
/// above-RAM-floor confirmation (REQ-555 BR-4b): the flag is the explicit
/// unattended stand-in for a typed answer, and it consumes no input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddConsent {
    /// `teton provider add` from a shell: no confirmation; the command line was
    /// the consent.
    Shell,
    /// `/provider add` in a session: confirm before the key is read.
    Session {
        /// The session's `--yes`, pre-answering the confirmation.
        assume_yes: bool,
    },
}

/// The default-no question the session asks before it reads a key.
///
/// It names everything the registration will do — the id, the kind, the model,
/// and the endpoint the request will reach, through the same masking renderer
/// every other endpoint line uses ([`escaped_endpoint`]; LESSON-529/535: a
/// preview is a surface, and a display that names a host the request will not
/// reach is worse than none) — and says what happens next, so a "y" is consent
/// to the read as well as to the write.
fn provider_add_question(settled: &SettledRegistration) -> String {
    let endpoint = settled
        .endpoint
        .as_deref()
        .map_or_else(|| "(no endpoint)".to_owned(), escaped_endpoint);
    let model = settled.model.as_deref().unwrap_or("(no model)");
    format!(
        "  register `{}` ({}, {model}) at {endpoint}? the key is read next, echo-off, into the \
         keychain [y/N] ",
        settled.id,
        kind_label(settled.kind)
    )
}

/// What the session says when the confirmation is declined: one line, and it
/// says exactly what did not happen — no registration, and no key read.
fn provider_add_declined_line(id: &str) -> String {
    format!("provider `{id}`: nothing registered; no key read.")
}

/// REQ-557 BR-1 / TASK-046: a remote provider MUST declare its model.
///
/// The one decision `provider add` can make before it has a connection, and the
/// reason it is a function rather than a line inside [`provider_add_on`]: the
/// shell wrapper asks it *before* `ensure_connected`, exactly as it always has,
/// so a command that was always going to fail still refuses without autostarting
/// a daemon. [`provider_add_on`] asks the same function first thing, so the
/// session — whose connection is already open — refuses on the same grounds.
/// One implementation, two call sites; a second copy of the predicate is the
/// mirrored-predicate shape LESSON-528 is about.
fn remote_provider_needs_model(
    id: &str,
    kind: ProviderKind,
    model: Option<&str>,
) -> Option<ProviderAddRefusal> {
    (!matches!(kind, ProviderKind::Local) && model.unwrap_or("").trim().is_empty())
        .then(|| ProviderAddRefusal::RemoteWithoutModel { id: id.to_owned() })
}

/// `teton provider add`: store the key in the keychain (BR-7), then register.
///
/// A connection-opening shell around [`provider_add_on`] plus the one check that
/// predates the connection ([`remote_provider_needs_model`]), and the mapping of
/// a [`ProviderAddRefusal`] back to the `bail!` a shell expects — non-zero exit,
/// the sentence on stderr (REQ-582 ADR-3).
fn run_provider_add(
    paths: &DaemonPaths,
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    // Before `ensure_connected`, because it always was: a `provider add` with no
    // `--model` has never started a daemon in order to refuse.
    if let Some(refusal) = remote_provider_needs_model(id, kind, model.as_deref()) {
        anyhow::bail!("{refusal}");
    }
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let keychain = keychain::default_keychain();
    match provider_add_on(
        &mut conn,
        &mut ctx,
        id,
        kind,
        endpoint,
        model,
        AddConsent::Shell,
        keychain.as_ref(),
    )? {
        Ok(()) => Ok(()),
        Err(refusal) => anyhow::bail!("{refusal}"),
    }
}

/// The body of `provider add`: the refusals, the composition echo, the
/// consent, the credential, and the registration (REQ-582 BR-2, ADR-3).
///
/// **The order of the steps is the specification.** Model check → duplicate
/// probe → endpoint settlement → session consent → credential → prior-key read
/// → payload → call → outcome report. BUG-155, BUG-171 and REQ-578 each pin a
/// different edge of it, and the comments below say which; nothing here may be
/// reordered without re-reading them.
///
/// `keychain` is the caller's ([`keychain::default_keychain`] from both the
/// shell wrapper and the session dispatcher) rather than built here, so the
/// composed read → store → `config/set` path — and the undo a refused
/// `config/set` owes the machine (BUG-171) — is drivable against
/// `MockKeychain` without a real login keychain in the loop (verify M4).
///
/// # Errors
///
/// Propagates a **transport** failure and nothing else: the socket going away
/// ends the session the way it ends `/cost`'s. Every decision *and* every
/// non-transport failure — the model check, the duplicate probe, an endpoint
/// the registration seam refuses (REQ-578 BR-5), a declined key, a keychain that
/// cannot store — travels as [`ProviderAddRefusal`], so the shell can `bail!`
/// with the sentence and the session can render it without ending (ADR-3;
/// verify m7).
#[allow(clippy::too_many_arguments)]
pub(crate) fn provider_add_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    model: Option<String>,
    consent: AddConsent,
    keychain: &dyn Keychain,
) -> anyhow::Result<Result<(), ProviderAddRefusal>> {
    // REQ-557 BR-1 / TASK-046: a remote provider MUST declare its model, and the
    // check runs BEFORE `read_secret` — otherwise the user types a credential
    // into a command that was always going to fail.
    if let Some(refusal) = remote_provider_needs_model(id, kind, model.as_deref()) {
        return Ok(Err(refusal));
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
    if let Ok(cfg) = conn.call(ConfigGetParams::default(), ctx)? {
        if cfg.snapshot.providers.iter().any(|p| p.id.0 == id) {
            return Ok(Err(ProviderAddRefusal::DuplicateId { id: id.to_owned() }));
        }
    }

    // REQ-578 BR-3/BR-4/BR-5: the last thing settled before a credential is
    // typed. Composition itself is a fact about the argv and could be computed
    // earlier, but the *refusal* and the *echo* belong here, after the
    // duplicate-id probe:
    //
    // - A command that is already refused for a better reason must keep being
    //   refused for that reason. `provider add deepseek --kind
    //   openai-compatible --model x` on an id that already exists answered
    //   "already registered" before this REQ and answers it still (BR-7) —
    //   a missing-`--endpoint` message there would be true, unhelpful, and a
    //   change to a shipped refusal.
    // - An echo printed before a refusal would say "endpoint stored as …"
    //   about a registration that is not happening.
    //
    // ADR-3 sketched this step one line earlier, before the probe; the shipped
    // order is that sketch minus those two consequences, and it keeps ADR-3's
    // reason intact — everything the user needs in order to decide whether to
    // type a key is on screen before they are asked for one.
    //
    // `endpoint` and `model` are moved into the settle step and never named
    // again in this function, and the payload builder below takes only what that
    // step returned. A reviewer showed the earlier shape — two live
    // `Option<String>`s, the raw one and the settled one, both in scope at the
    // `build_provider_registration` call — could be mutated to register the raw
    // argv while still echoing the composed value, with all 35 e2e tests and the
    // whole unit suite green (REQ-578 verify). Passing the wrong value now means
    // changing `registration_params`' signature, and
    // `the_endpoint_that_is_echoed_is_the_endpoint_that_is_registered` drives
    // both of these functions, so a mutation inside either one fails.
    // The seam's refusal is a sentence composed for a user, and it already ends
    // "nothing was changed and no credential was read" — true here, since the
    // key is not asked for until the step after this one. A refusal rather than
    // an `Err` so a mistyped `--endpoint` does not end the session (m7).
    let settled = match settle_registration(id, kind, endpoint, model, &mut *ctx.surface) {
        Ok(settled) => settled,
        Err(refused) => return Ok(Err(ProviderAddRefusal::Endpoint(format!("{refused:#}")))),
    };

    // Local providers have no credential; every remote kind requires a key.
    let needs_key = !matches!(kind, ProviderKind::Local);

    // REQ-582 verify M1: the session confirms **before** the key is read, and
    // only when a key is about to be. Everything the user needs in order to
    // decide is on screen by now — the echo of the composed endpoint, the
    // cleartext warning — and the question names the settled registration once
    // more, so a "y" typed here is consent to exactly what will be stored. A
    // shell asks nothing: its command line was the consent, and its bytes are
    // pinned by the e2e suite. A local registration asks nothing either: there
    // is no key to protect a pasted second line from becoming, and the write
    // itself already passed the typed-input gate.
    if needs_key {
        if let AddConsent::Session { assume_yes } = consent {
            let confirmed = assume_yes
                || matches!(
                    ctx.prompter.ask(&provider_add_question(&settled)),
                    Some(answer) if prompt::is_yes(&answer)
                );
            if !confirmed {
                ctx.surface
                    .line(LineKind::Info, &provider_add_declined_line(id));
                return Ok(Ok(()));
            }
        }
    }

    // The prompter is the caller's (ADR-3): `StdinPrompter` under the shell's
    // passive context, and the session's own dialogue prompter under `/provider
    // add` — echo-off on both, because that is [`read_secret`]'s choice and not
    // this call site's.
    let secret = if needs_key {
        // The only error `read_secret` has is "nothing was typed", so it becomes
        // the refusal rather than an `Err` that would end a session.
        match read_secret(id, &mut *ctx.prompter) {
            Ok(secret) => Some(secret),
            Err(_) => return Ok(Err(ProviderAddRefusal::NoKey)),
        }
    } else {
        None
    };
    // What the store inside `build_provider_registration` is about to displace,
    // read in the same breath — the store destroys the answer, and a rejected
    // registration owes the machine an undo decided by exactly this (BUG-171).
    let prior = secret.as_ref().map(|_| PriorKey::read(keychain, id));
    // A store the keychain refuses leaves nothing behind — the store is what
    // failed — so there is nothing to undo, only a sentence to render (m7).
    let prepared = match registration_params(&settled, keychain, secret.as_deref()) {
        Ok(prepared) => prepared,
        Err(failed) => {
            return Ok(Err(ProviderAddRefusal::KeychainStore(format!(
                "{failed:#}"
            ))))
        }
    };
    let PreparedRegistration { params, auth } = prepared;

    // Bound rather than `?`-ed past: a transport failure is not the same event
    // as a daemon that answered "no" — the registration may or may not have
    // landed, and a key this run stored must be accounted for out loud on every
    // path (BUG-171).
    match conn.call(params, ctx) {
        Ok(outcome) => {
            report_registration_outcome(
                outcome,
                id,
                kind,
                &auth,
                prior.as_ref(),
                keychain,
                ctx.surface,
            );
            Ok(Ok(()))
        }
        Err(transport) => {
            if prior.is_some() {
                ctx.surface
                    .line(LineKind::Notice, &registration_unanswered_line(id, &auth));
            }
            Err(transport)
        }
    }
}

/// `teton provider test <id>`: one consented call to a registered provider
/// (REQ-581 BR-7, architecture ADR-5).
///
/// The **one** subcommand under `teton provider` that opens a session, and the
/// reason is that it is not a read: it sends. `provider/test` is gated on
/// session attachment precisely so a foreign connection — or a `teton provider
/// test … --yes` the model spawned through the shell tool, which REQ-569's
/// ancestry gate already keeps out of the user's sessions — cannot make the
/// user's provider bill them. Creating a session here is also what gives the
/// resulting ledger row a session to belong to (BR-5). The session ends with the
/// process, exactly as `teton`'s own does.
///
/// Everything after the connection is [`provider_test_ui`]'s, so this subcommand
/// and the in-session `/provider test` cannot diverge (BR-7, LESSON-441's rule:
/// one consent gate, one implementation).
///
/// # Errors
///
/// Propagates a transport error. A daemon that *answers* — including a refusal
/// to create the session — is reported on the surface and returns `Ok`.
fn run_provider_test(
    paths: &DaemonPaths,
    id: &str,
    assume_yes: bool,
    session_root: Option<&Path>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    // The passive context reads `typed_input` at the edge exactly as the session
    // path does, so the gate answers the same question here as it does in a
    // session and no handler re-derives it.
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    let typed_input = ctx.typed_input;

    let created = conn.call(
        SessionCreateParams {
            mode: SessionMode::Freeform,
            phase: None,
            // BUG-147: the daemon runs under launchd (cwd `/`); a session it
            // mints must still be anchored to THIS terminal's directory — or
            // to the `--cwd` named instead of it (REQ-583 BR-6).
            cwd: session_root.map(Path::to_path_buf),
        },
        &mut ctx,
    )?;
    let session_id = match created {
        Ok(res) => res.session_id,
        Err(err) => {
            ctx.surface.line(
                LineKind::Error,
                &format!("could not start a session for the test: {}", err.message),
            );
            return Ok(());
        }
    };
    ctx.session_id = Some(session_id.clone());
    ctx.state.session_id = Some(session_id.clone());

    let mut io = provider_test_ui::DaemonIo::new(&mut conn, &mut ctx);
    provider_test_ui::run(&mut io, &session_id, id, assume_yes, typed_input)
}

/// `teton provider list`.
fn run_provider_list(paths: &DaemonPaths) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    provider_list_on(&mut conn, &mut ctx)
}

/// The body of `provider list`: one `config/get`, one [`render_config`]
/// (REQ-582 BR-2).
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn provider_list_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    match conn.call(ConfigGetParams::default(), ctx)? {
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
    boundary_add_on(&mut conn, &mut ctx, &glob, mode)
}

/// The body of `boundary add`: one `config/set`, one set of outcomes (REQ-582
/// BR-2).
///
/// The daemon-side gates are the twin's — this sends the same
/// `SetPrivacyBoundary` params from either surface, so the presence attestation
/// and the ancestry gate apply to `/boundary add` exactly as they do to
/// `teton boundary add` (BR-6).
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn boundary_add_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    glob: &str,
    mode: PrivacyMode,
) -> anyhow::Result<()> {
    let params = ConfigSetParams {
        update: ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
            path_glob: glob.to_owned(),
            mode,
        }),
    };
    match conn.call(params, ctx)? {
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
    boundary_list_on(&mut conn, &mut ctx)
}

/// The body of `boundary list`: one `config/get`, one listing (REQ-582 BR-2).
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn boundary_list_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<()> {
    match conn.call(ConfigGetParams::default(), ctx)? {
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
///
/// `pub(crate)` since REQ-582's verify pass (m6): a `teton policy set …` typed at
/// the session prompt walks clap's tree to this hidden leaf, and the classifier
/// answers with this sentence rather than a generic "no session form" — the
/// user's mistake is the retired axis, and this is the one text that says so.
pub(crate) const POLICY_SET_RETIRED: &str =
    "`teton policy set <phase> <provider>` is retired. Routing \
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
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    policy_set_tier_on(&mut conn, &mut ctx, tier, &provider, fallback.as_deref())
}

/// The body of `policy set-tier` (REQ-582 BR-2).
///
/// The payload is built **here** rather than at each surface, so `/policy
/// set-tier` and `teton policy set-tier` cannot send different params for the
/// same words — which is the half of BR-6 the client owns; the gates the daemon
/// applies to those params are unchanged either way.
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn policy_set_tier_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    tier: Tier,
    provider: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    policy_bind_on(
        conn,
        ctx,
        ConfigUpdate::SetTierBinding(TierBindingConfig {
            tier,
            provider_id: ProviderId::from(provider),
            fallback_id: fallback.map(ProviderId::from),
        }),
        &format!("the '{tier}' tier"),
        provider,
        fallback,
    )
}

/// `teton policy set-category`.
fn run_policy_set_category(
    paths: &DaemonPaths,
    category: ConfigurableCategory,
    provider: String,
    fallback: Option<String>,
) -> anyhow::Result<()> {
    let mut surface = stdout_surface();
    let mut conn = client::ensure_connected(paths, &mut surface)?;
    let mut state = SessionState::new();
    let mut prompter = StdinPrompter::new();
    let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
    policy_set_category_on(
        &mut conn,
        &mut ctx,
        category,
        &provider,
        fallback.as_deref(),
    )
}

/// The body of `policy set-category` (REQ-582 BR-2). See
/// [`policy_set_tier_on`] for why the payload is built here.
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn policy_set_category_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    category: ConfigurableCategory,
    provider: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    policy_bind_on(
        conn,
        ctx,
        ConfigUpdate::SetCategoryBinding(CategoryBindingConfig {
            name: category,
            provider_id: ProviderId::from(provider),
            fallback_id: fallback.map(ProviderId::from),
        }),
        &format!("the '{category}' category"),
        provider,
        fallback,
    )
}

/// The shared body of `set-tier` and `set-category`: one round trip, one set of
/// outcomes, one sentence shape. The two differ only in what they bind.
///
/// Takes the caller's connection and context (REQ-582 ADR-3), so the four
/// call sites — two subcommands and their two session rows — are one
/// implementation of the write and one of the sentences it renders.
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn policy_bind_on(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    update: ConfigUpdate,
    what: &str,
    provider: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    let fallback_note = fallback.map_or_else(String::new, |f| format!(" (fallback {f})"));
    match conn.call(ConfigSetParams { update }, ctx)? {
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
    policy_show_on(&mut conn, &mut ctx)
}

/// The body of `policy show`: one `config/get`, one [`render_policy`] (REQ-582
/// BR-2).
///
/// # Errors
///
/// Propagates a transport error; a daemon that answers is reported on the
/// surface and returns `Ok`.
pub(crate) fn policy_show_on(conn: &mut Connection, ctx: &mut UiContext<'_>) -> anyhow::Result<()> {
    match conn.call(ConfigGetParams::default(), ctx)? {
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

/// An endpoint as it should be **shown**, with any `user:password@` userinfo
/// replaced (REQ-578 verify).
///
/// The stored value is untouched: composition decides paths, and an address the
/// user typed is dialled as typed. What changes is the *rendering*, because
/// every line the CLI prints an endpoint into goes somewhere a credential should
/// not — a terminal scrollback, a session recording, the output a user pastes
/// into a bug report. Every such line goes through here: the registration echo,
/// doctor's advisory, and `provider list`'s table.
///
/// Replaced rather than deleted, so the rendered URL still says a credential was
/// there. A line that silently dropped it would claim to be showing "that exact
/// URL" while showing a different one, which is the specific failure the echo
/// exists to prevent.
///
/// **The authority ends at a backslash too**, and that is the load-bearing part.
/// WHATWG reads `\` as `/` in a special scheme, so `https://evil.example\@127.0.0.1/x`
/// is a request to `evil.example` — while a splitter that stopped at `/?#` alone
/// would read the whole thing as an authority, take the userinfo off at the last
/// `@`, and *render* `127.0.0.1`. That is a display that names a host the request
/// will not reach, which is worse than no redaction at all. The registration seam
/// can no longer produce this shape (it is refused by
/// [`teton_core::is_absolute_http_url`] before anything is stored), but doctor
/// renders whatever a hand-edited config holds, so the reading has to be right
/// here as well. `\` is the only code point where the two readings diverge.
///
/// `pub(crate)` because "every such line" includes lines outside this module:
/// [`provider_test_ui`]'s preview echoes the same stored endpoint back before
/// the user consents to a call (REQ-581 BR-2). A second masker over there would
/// be a second reading of the authority, which is the specific failure the
/// backslash paragraph above is about.
pub(crate) fn displayed_endpoint(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    // The *last* `@` ends the userinfo: a password may contain one, and the
    // authority ends at the last — the same reading `teton_core`'s `url_host`
    // takes of the same region.
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_owned(),
    }
}

/// An endpoint as it should be shown when it may hold bytes the registration
/// seam would have refused (REQ-578 verify).
///
/// [`displayed_endpoint`] plus escaping for TAB, LF and CR. `provider add`
/// refuses those outright, so this spelling exists for the one path that cannot
/// refuse anything: doctor, reading a config somebody wrote by hand. Printing a
/// raw control byte into a terminal is how a stored endpoint gets to move the
/// cursor, and a `\r` in particular can overwrite the line that was about to
/// name it.
///
/// The escape character is deliberately not itself escaped: a doubled `\` would
/// misrender the one shape this display most needs to be readable — an authority
/// carrying a backslash — and nothing round-trips this string, so the residual
/// ambiguity between a literal `\t` and an escaped TAB costs nothing.
fn escaped_endpoint(url: &str) -> String {
    displayed_endpoint(url)
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// The one line `provider add` says about an endpoint it did not store exactly
/// as typed (REQ-578 BR-4).
///
/// It names the stored value and what will be done with it, because the thing a
/// user has to be able to catch is a URL that is *plausible and wrong*: the
/// kind-level rule cannot know whether a `/v1` belongs to a given vendor's base
/// URL (DeepSeek's documented base has none, OpenAI's has one), so a bare origin
/// for a `/v1`-family vendor composes to an address that vendor does not serve
/// (ADR-2's recorded known limit). This line is that limit's mitigation, which
/// is why it is emitted before a credential is typed rather than after a 404.
fn endpoint_echo_line(stored: &str) -> String {
    format!(
        "endpoint stored as {} — that exact URL is what Teton will POST.",
        displayed_endpoint(stored)
    )
}

/// The warning `provider add` gives before asking for a key that will travel
/// unencrypted (REQ-578 verify).
fn cleartext_endpoint_line(id: &str, host: &str) -> String {
    format!(
        "`{id}`'s endpoint is `http://`, so the API key you are about to type travels to {host} \
         in the clear — every hop between this machine and that host can read it. Use `https://` \
         if {host} serves it. (A loopback address is exempt: nothing leaves the machine.)"
    )
}

/// Bytes an endpoint may not contain, because the terminal and the network stack
/// would disagree about what the URL is (REQ-578 verify).
///
/// TAB, LF and CR are *deleted* by URL parsers (WHATWG strips them from every
/// position) and *rendered as spacing* by a terminal. So an endpoint carrying one
/// would be echoed back as a string that is not the string Teton dials, with the
/// difference invisible on screen — which defeats the one mitigation BR-4 offers
/// for a composed URL. A paste that spanned a line break is the usual source.
const FORBIDDEN_ENDPOINT_BYTES: [char; 3] = ['\t', '\n', '\r'];

/// The endpoint this registration will persist, settled before anything
/// credential-shaped happens (REQ-578 BR-1/BR-3/BR-4/BR-5).
///
/// Six things, in this order, and the order is the point:
///
/// 1. **Refuse bytes that render differently from how they dial.** See
///    [`FORBIDDEN_ENDPOINT_BYTES`]: an endpoint whose echo cannot be trusted is
///    worse than no echo, so it is refused before anything else happens to it.
///    Ahead of the shape check below, which would also refuse these — because
///    "there is a tab in this URL" is a better sentence than "this is not an
///    absolute URL" for a value whose only fault is a tab.
/// 2. **Refuse a shape that is not an absolute `http(s)://` URL.** The
///    predicate is `teton_core`'s own [`is_absolute_http_url`], the one the
///    `[web]` search endpoint is held to. This is the check that removes the
///    family of strings a URL parser reads as an authority and a string-splitter
///    does not — `http:/host`, `http:\\host`, `http:/\host`, `http:\/host` (all
///    of which `url` resolves to `http://host`), `http:///v1` (no host at all),
///    and `https://evil.example\@127.0.0.1/x`, whose host is `evil.example`
///    under WHATWG and `127.0.0.1` under a naive read. Everything below this
///    line, including the cleartext warning and every rendering, may therefore
///    assume a URL whose host it can name.
/// 3. **Compose.** A vendor *base* URL becomes the absolute request URL Teton
///    POSTs verbatim, and `--kind anthropic` with no `--endpoint` gets the
///    official Messages URL written explicitly into config (BR-3). The rule
///    itself lives in `teton_core::compose_endpoint` and nowhere else.
/// 4. **Refuse what cannot work.** A remote registration with no endpoint is
///    the BUG-170 sequence: the daemon's validator refuses it — *after* the
///    user's key has been read and stored. The predicate here is the validator's
///    own (`kind.is_remote() && endpoint.trim().is_empty()`), so this moves an
///    existing refusal in front of the prompt and names the flag that fixes it.
/// 5. **Echo, when the stored value is not what was typed.** Only then, and
///    only for a registration that is still going ahead.
/// 6. **Warn when the key would travel in the clear.** Last, so it sits
///    immediately above the prompt it is about.
///
/// **Scope of steps 1 and 2 (BR-6).** They are refusals at the *registration
/// seam*, not new validity conditions: `Config::validate` is untouched, so a
/// config carrying either shape — hand-written, or written before this pass —
/// still loads and still starts the daemon. Doctor is that config's surface.
/// What changed is only that the CLI no longer helps a user create one.
///
/// Split out of [`run_provider_add`] so all six are drivable from a unit test
/// with a recording surface — the real flow needs a daemon connection, and the
/// ordering claim (BR-5) is exactly the kind of property that regresses when a
/// later edit moves one line.
///
/// The six steps themselves live in [`settle_endpoint_text`]; what is left here
/// is the rendering. `/provider setup` calls that core directly (REQ-579 ADR-8):
/// one compose-and-echo seam, not two that agree until the day they do not
/// (LESSON-528).
fn settle_endpoint(
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    surface: &mut dyn Surface,
) -> anyhow::Result<Option<String>> {
    let settled =
        settle_endpoint_text(id, kind, endpoint.as_deref()).map_err(anyhow::Error::msg)?;
    // The echo first, the warning second: the warning is about the key prompt
    // that follows, so it sits immediately above it (step 6).
    if let Some(echo) = &settled.echo {
        surface.line(LineKind::Info, echo);
    }
    if let Some(warning) = &settled.cleartext_warning {
        surface.line(LineKind::Notice, warning);
    }
    Ok(settled.stored)
}

/// What [`settle_endpoint_text`] decided: the value to persist, and the two
/// lines a caller owes the user before it asks for a credential.
///
/// `echo` and `cleartext_warning` are *content*, not output. The surface a
/// caller renders them on is its own — `teton provider add` writes them to
/// stdout, `/provider setup` writes them to the session's `Surface` — and
/// keeping them as data is what lets both callers share the decision rather
/// than each re-deriving it (REQ-579 ADR-8).
pub(crate) struct SettledEndpoint {
    /// The absolute request URL to persist, or `None` when there is nothing to
    /// store (the `Local` kind).
    pub stored: Option<String>,
    /// The "endpoint stored as …" line, when the stored value is not what was
    /// typed (REQ-578 BR-4). `None` when they are the same string.
    pub echo: Option<String>,
    /// The "this key travels in the clear" line, when the endpoint is `http://`
    /// to a non-loopback host. `None` otherwise.
    pub cleartext_warning: Option<String>,
}

/// The endpoint this registration will persist, decided and nothing else
/// (REQ-578 BR-1/BR-3/BR-4/BR-5; REQ-579 ADR-8).
///
/// The pure half of [`settle_endpoint`] — same six steps, same order, same
/// sentences — returning the refusal as a `String` rather than bailing and the
/// two advisory lines as data rather than as surface calls. `/provider setup`
/// is the second caller, and it has a different surface, a different abort
/// (a rendered line, not a process exit) and no argv to name; a second copy of
/// this logic there is exactly the mirrored predicate LESSON-528 is about.
///
/// # Errors
///
/// The refusal sentence, ready to render. Every one of them ends by saying that
/// nothing was changed and no credential was read, which is true of both
/// callers: each runs this before it asks for a key.
pub(crate) fn settle_endpoint_text(
    id: &str,
    kind: ProviderKind,
    endpoint: Option<&str>,
) -> Result<SettledEndpoint, String> {
    // Both checks read the value as *supplied* rather than the composed one:
    // what is at stake is the string the user typed and can see, and composition
    // only ever appends a path to it.
    if let Some(supplied) = endpoint {
        if supplied.contains(FORBIDDEN_ENDPOINT_BYTES) {
            return Err(format!(
                "provider `{id}`: the `--endpoint` value contains a tab, newline or carriage \
                 return. Teton refuses it rather than guessing which one you meant: URL parsers \
                 *delete* those bytes while a terminal *renders* them as spacing, so the address \
                 shown back to you would not be the address Teton dials and nothing on screen \
                 would say so. Re-paste the URL without them — a stray one usually comes from a \
                 copy that spanned a line break. Nothing was changed and no credential was read."
            ));
        }
    }

    // Blank is "absent" here exactly as it is inside `compose_endpoint`, so a
    // `--endpoint ""` on `--kind anthropic` still reaches BR-3's default rather
    // than being shape-checked and refused.
    let supplied = endpoint.map(str::trim).filter(|v| !v.is_empty());
    if let Some(supplied) = supplied {
        if !matches!(kind, ProviderKind::Local) && !is_absolute_http_url(supplied) {
            return Err(format!(
                "provider `{id}`: `--endpoint {supplied}` is not an absolute `http://` or \
                 `https://` URL with a host. Teton refuses it rather than storing it, because \
                 several near-misses are read one way by a URL parser and another way by anything \
                 that merely looks at the string — `http:/host` and `http:\\host` are requests to \
                 `host`, and a backslash in the authority (`https://a\\@b/`) moves the host to the \
                 part before it. An address Teton cannot render the same way it dials is one it \
                 will not register. Pass the URL with its scheme, e.g. \
                 `--endpoint https://api.moonshot.ai/v1`. Nothing was changed and no credential \
                 was read."
            ));
        }
    }

    let composed = compose_endpoint(CoreProviderKind::from(kind), endpoint);

    if !matches!(kind, ProviderKind::Local)
        && composed.stored.as_deref().unwrap_or("").trim().is_empty()
    {
        // The completion sentence is true only for a kind Teton knows a request
        // path for. `custom` names an operator's own adapter, so there is
        // nothing to complete and offering to would send the user looking for a
        // behaviour that does not exist.
        let completion = if canonical_request_path(CoreProviderKind::from(kind)).is_some() {
            "Your vendor's documented base URL is enough (e.g. `--endpoint \
             https://api.moonshot.ai/v1`) — Teton completes it to the request URL and tells you \
             what it stored."
        } else {
            "Nothing is completed for a `custom` kind — Teton does not know your adapter's \
             protocol — so pass the full request URL your gateway serves."
        };
        return Err(format!(
            "provider `{id}` is a remote provider and must declare the URL it calls: pass \
             `--endpoint <url>`. {completion} Nothing was changed and no credential was read."
        ));
    }

    // Remote kinds only. The echo's sentence is "that exact URL is what Teton
    // will POST", and the on-device tier POSTs nothing anywhere — so a `Local`
    // registration whose endpoint was merely trimmed (the one way `changed` can
    // fire for a kind that is never composed) would be told something false
    // about a value the daemon ignores.
    let echo = (composed.changed && !matches!(kind, ProviderKind::Local))
        .then(|| composed.stored.as_deref().map(endpoint_echo_line))
        .flatten();

    // Last, and only for a kind that is about to be asked for a credential: a
    // local provider has none, so there is nothing to expose and nothing to say.
    //
    // `teton_core`'s own predicate and host reader, not a copy of them — the
    // shape check above is exactly the precondition they are written against, so
    // the CLI meets the same contract the `[web]` validator does. A remote
    // endpoint that reaches here has a host by construction, so the `and_then`
    // is total in practice; it is written as an option chain because `url_host`
    // is honest about a shape this path can no longer produce.
    let cleartext_warning = (!matches!(kind, ProviderKind::Local))
        .then(|| {
            composed
                .stored
                .as_deref()
                .filter(|stored| is_cleartext_to_a_remote_host(stored))
                .and_then(url_host)
                .map(|host| cleartext_endpoint_line(id, host))
        })
        .flatten();

    Ok(SettledEndpoint {
        stored: composed.stored,
        echo,
        cleartext_warning,
    })
}

/// A registration whose endpoint has been settled: everything `config/set` will
/// be told, and nothing the user typed (REQ-578 verify).
///
/// The type exists to put one class of defect behind a signature change. Before
/// it, `run_provider_add` held both the raw `--endpoint` argv and the settled
/// value as two live `Option<String>`s, and passing the wrong one into
/// [`build_provider_registration`] compiled, ran, and survived the entire test
/// suite — the echo said one URL and the daemon stored another, with every AC
/// still green because each half was asserted separately. Now the raw value is
/// consumed by [`settle_registration`] and [`registration_params`] is handed
/// nothing else, so the wrong string is not in scope at the call that builds the
/// payload.
struct SettledRegistration {
    id: String,
    kind: ProviderKind,
    /// The absolute request URL to persist — the value the echo named.
    endpoint: Option<String>,
    model: Option<String>,
}

/// The registration and the keychain reference it carries.
struct PreparedRegistration {
    params: ConfigSetParams,
    /// What was stored for this provider, or `—` for a kind with no credential.
    /// BUG-171's reporting needs it after `params` has been moved onto the wire.
    auth: String,
}

/// Settle the endpoint and take ownership of the registration's fields.
///
/// The consuming half of [`SettledRegistration`]: `endpoint` and `model` come in
/// by value and do not come back out except inside the struct.
fn settle_registration(
    id: &str,
    kind: ProviderKind,
    endpoint: Option<String>,
    model: Option<String>,
    surface: &mut dyn Surface,
) -> anyhow::Result<SettledRegistration> {
    let endpoint = settle_endpoint(id, kind, endpoint, surface)?;
    Ok(SettledRegistration {
        id: id.to_owned(),
        kind,
        endpoint,
        model,
    })
}

/// Turn a settled registration into the `config/set` payload, storing any secret
/// in the keychain first so only the reference travels onward (BR-7).
///
/// The only reader of [`SettledRegistration::endpoint`], and the seam a test can
/// stand at to compare what was echoed against what will be sent.
fn registration_params(
    settled: &SettledRegistration,
    keychain: &dyn Keychain,
    secret: Option<&str>,
) -> anyhow::Result<PreparedRegistration> {
    let config = build_provider_registration(
        &settled.id,
        settled.kind,
        settled.endpoint.clone(),
        settled.model.clone(),
        keychain,
        secret,
    )?;
    let auth = config.auth_ref.clone().unwrap_or_else(|| "—".to_owned());
    Ok(PreparedRegistration {
        params: ConfigSetParams {
            update: ConfigUpdate::RegisterProvider(config),
        },
        auth,
    })
}

/// What `teton doctor` says about a stored endpoint that looks like a vendor
/// *base* URL, or `None` for one it has nothing to say about (REQ-578 BR-6,
/// ADR-4).
///
/// The predicate is [`compose_endpoint`] itself, not a second reading of BR-2's
/// classes: an endpoint the registration seam would still *complete* is, by
/// definition, one of the class (b) shapes — a bare origin, a bare `/`, or a
/// bare `/v1`. Custom paths (class (c)) and endpoints that already carry the
/// kind's request path (class (a)) compose to themselves and are therefore
/// silent here, which is the half of AC-5 that keeps this from becoming a scold:
/// a gateway serving chat completions at `/llm/proxy` is a first-class
/// deployment, not a mistake.
///
/// "Composes to itself" is read whitespace-insensitively rather than off
/// `ComposedEndpoint::changed`, because that flag also fires when trimming alone
/// moved the bytes — which is the right trigger for the registration echo (the
/// user typed something that was not stored) and the wrong one here (a stored
/// endpoint with a stray blank has not gained a request path and will not 404
/// for want of one).
///
/// **Advisory, never a fault.** `Config::validate` gains no new fatal class
/// (BR-6) and doctor's exit status is untouched: a bare origin can even be
/// right, if a host really does serve the protocol at its root. What it cannot
/// be is *invisible* — since REQ-577 the stored endpoint is POSTed verbatim, so
/// a config hand-edited to a base URL fails on the first turn with a 404 that
/// names nothing. This line is that 404's cause, said in advance.
///
/// A registration made through `provider add` is composed at the seam and so is
/// never flagged; what reaches this pass is a config somebody wrote by hand, or
/// one written before this composition existed.
///
/// **What the line may claim.** Only what Teton would do — never what the
/// vendor serves. For a bare `/v1` base the composed form is the one address the
/// rule can be sure of, and the sentence says so plainly. For a bare *origin* on
/// an OpenAI-compatible provider it cannot: `/chat/completions` is right for
/// DeepSeek and wrong for OpenAI, whose base carries a `/v1`, and this pass has
/// no way to tell the two hosts apart (ADR-2's recorded known limit). That case
/// therefore names the composed form *and* the `/v1` alternative and sends the
/// user to their vendor's docs, because an advisory that stated a false fact
/// about a real vendor would be worse than the 404 it is trying to explain — it
/// was live-observed advising `https://api.openai.com/chat/completions`, which
/// 404s. Anthropic has no such ambiguity: its canonical path *is* versioned, so
/// a bare origin composes to the one URL that vendor documents.
fn base_url_advisory(provider: &ProviderConfig) -> Option<String> {
    let endpoint = provider.endpoint.as_deref()?;
    let full = compose_endpoint(CoreProviderKind::from(provider.kind), Some(endpoint)).stored?;
    // Whitespace-insensitively, because composition trims its input: an endpoint
    // that differs from its composition only by surrounding blanks has gained no
    // request path, and advising on it would be a scold about bytes TOML kept
    // rather than about a URL that answers 404.
    if full == endpoint.trim() {
        return None;
    }

    // The `/v1` alternative, derived from the module's own canonical path rather
    // than by re-parsing the URL: strip the path composition appended and what
    // is left is the stem the user supplied. A stem that already ends in `/v1`,
    // or a canonical path that carries its own version segment, leaves nothing
    // ambiguous — so there is no second form to offer.
    let canonical = canonical_request_path(CoreProviderKind::from(provider.kind))?;
    let versioned = full
        .strip_suffix(canonical)
        .filter(|stem| !canonical.starts_with("/v1") && !stem.ends_with("/v1"))
        .map(|stem| format!("{}/v1{canonical}", displayed_endpoint(stem)));

    let id = &provider.id;
    // Escaped, not merely redacted: this pass reads whatever a hand-edited
    // config holds, and `provider add`'s refusal of TAB/LF/CR does not reach it.
    // Rendered from the *trimmed* value, so the advisory names the address
    // composition actually reasoned about rather than one with invisible padding
    // around it.
    let shown = escaped_endpoint(endpoint.trim());
    let full = escaped_endpoint(&full);
    let completion = match versioned {
        Some(versioned) => format!(
            "Teton would store `{full}`, but many vendors serve this under `/v1` (e.g. \
             `{versioned}`) — check your vendor's docs, then re-add the provider or edit \
             config.toml to use the right one."
        ),
        None => format!(
            "Teton would store `{full}` — re-add the provider, or edit config.toml to use it."
        ),
    };
    Some(format!(
        "provider `{id}`: the stored endpoint `{shown}` looks like a vendor base URL. Teton \
         POSTs the stored endpoint verbatim and joins nothing onto it at call time, so it has to \
         be the full request URL. {completion} This is advice, not a fault — the config is valid \
         and doctor's status is unchanged — but if `{id}` answers 404 on its first turn, that is \
         the reason."
    ))
}

/// Doctor's pass over the configured providers (REQ-578 ADR-4).
///
/// One [`LineKind::Notice`] per class-(b)-shaped endpoint and nothing at all for
/// the rest, emitted after the provider listing so the advice sits under the
/// thing it is about. Reuses the composition rule rather than re-deriving it:
/// the "full form" this names has to be the same string `provider add` would
/// have stored, or the advice would send a user somewhere the product itself
/// would not.
fn advise_on_base_url_endpoints(providers: &[ProviderConfig], surface: &mut dyn Surface) {
    for provider in providers {
        if let Some(advice) = base_url_advisory(provider) {
            surface.line(LineKind::Notice, &advice);
        }
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

/// Render the daemon's answer to a provider registration, and settle the
/// keychain entry the attempt stored (BUG-171).
///
/// `prior` is `Some` exactly when this run stored a credential; it is what the
/// account held *before* that store, and it decides what a rejection owes the
/// machine. The flow-agnostic three-state undo lives in [`PriorKey::undo`] —
/// what belongs here is *when* it runs (only on a rejection: `config/set`
/// validates before it persists, so a refused registration leaves the stored
/// entry referenced by nothing) and the sentences rendered about it.
///
/// Split from [`run_provider_add`] so every arm is drivable from a test with a
/// mock keychain and a recording surface — the real flow's connection cannot
/// be, and a rollback is exactly the kind of branch that regresses silently.
fn report_registration_outcome(
    outcome: Result<ConfigSetResult, RpcError>,
    id: &str,
    kind: ProviderKind,
    auth: &str,
    prior: Option<&PriorKey>,
    keychain: &dyn Keychain,
    surface: &mut dyn Surface,
) {
    match outcome {
        // The keychain sentence is claimed only when a key was stored — a local
        // provider stored nothing, and the old unconditional line told it a
        // key was in the keychain under ref `—`.
        Ok(res) if res.applied => surface.line(
            LineKind::Info,
            &match prior {
                Some(_) => format!(
                    "provider `{id}` registered ({}). Key stored in the OS keychain (ref {auth}); \
                     no key written to disk.",
                    kind_label(kind)
                ),
                None => format!("provider `{id}` registered ({}).", kind_label(kind)),
            },
        ),
        Ok(_) => {
            surface.line(
                LineKind::Notice,
                &format!("provider `{id}`: the daemon did not apply the registration."),
            );
            // "Did not apply" is true of the config and **false of the
            // keychain** when this run stored a key — and no delete is licensed
            // here: an already-present identical registration may reference the
            // entry, so taking it out could break a working provider.
            if prior.is_some() {
                surface.line(
                    LineKind::Notice,
                    &format!(
                        "The key you typed is in the OS keychain (ref {auth}) and was left \
                         there — a registration the daemon already had may reference it; \
                         `teton provider list` shows whether `{id}` is configured."
                    ),
                );
            }
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => surface.line(
            LineKind::Notice,
            &match prior {
                // The key is deliberately *kept*: registration is pending, not
                // refused, and the entry is what that registration will
                // reference once a daemon that implements config/set is running.
                Some(_) => format!(
                    "provider `{id}`: key stored in the OS keychain (ref {auth}); this daemon \
                     build does not implement config/set yet, so registration is pending \
                     TASK-013."
                ),
                None => format!(
                    "provider `{id}`: this daemon build does not implement config/set yet, so \
                     registration is pending TASK-013."
                ),
            },
        ),
        Err(err) => {
            let cleanup = prior.map(|prior| prior.undo(keychain));
            surface.line(
                LineKind::Error,
                &format!("provider `{id}` registration rejected: {}", err.message),
            );
            if let Some(cleanup) = cleanup {
                surface.line(LineKind::Notice, &provider_cleanup_line(id, &cleanup));
            }
        }
    }
}

/// What the rejection path did about the keychain entry the attempt stored —
/// said out loud, including when it did nothing (`/web setup`'s `cleanup_line`,
/// re-worded for an account named by the provider id).
///
/// A failure to clean up is reported rather than swallowed: the user is the
/// only one who can act on the keychain by hand, and a credential left in a
/// state they were never told about is exactly the residue BUG-171 was. Each
/// arm that leaves an entry behind therefore ends in the command that finishes
/// the job it could not.
fn provider_cleanup_line(id: &str, cleanup: &Cleanup) -> String {
    match cleanup {
        Cleanup::Deleted(Ok(())) => {
            "the key that was stored for this attempt has been removed from your keychain; \
             nothing was registered and nothing references it."
                .to_owned()
        }
        Cleanup::Deleted(Err(err)) => format!(
            "the key stored for this attempt could not be removed from your keychain ({err}) — \
             it is unreferenced, and `security delete-generic-password -s teton -a {id}` \
             clears it."
        ),
        Cleanup::Restored(Ok(())) => format!(
            "the keychain entry `{id}` has been put back to the credential it held before \
             this attempt."
        ),
        Cleanup::Restored(Err(err)) => format!(
            "the credential the keychain entry `{id}` held before this attempt could not be \
             put back ({err}) — the entry now holds the key you just typed. \
             `security add-generic-password -U -s teton -a {id} -w` restores it by hand."
        ),
        Cleanup::LeftInPlace(why) => format!(
            "your keychain could not be read before this attempt ({why}), so the key you typed \
             was left in the `{id}` entry rather than risk removing a credential something \
             else still uses — `security delete-generic-password -s teton -a {id}` removes it \
             once you have checked nothing does."
        ),
    }
}

/// What a registration call the daemon never answered says about the stored key
/// (BUG-171, mirroring `/web setup`'s ambiguous-commit treatment).
///
/// Deliberately **no** keychain mutation on this path: the registration either
/// landed or did not, this process cannot tell, and either undo is destructive
/// in one of the two states — a delete orphans a landed registration, a restore
/// resurrects a key the user meant to replace. The user gets the ambiguity
/// itself, with the command that resolves it.
fn registration_unanswered_line(id: &str, auth: &str) -> String {
    format!(
        "the daemon did not answer the registration call, so provider `{id}` may or may not be \
         registered — `teton provider list` shows which. The key you typed is in the OS keychain \
         (ref {auth}) and was left there: taking it out would break the provider if the \
         registration did land; if it did not, \
         `security delete-generic-password -s teton -a {id}` removes it."
    )
}

/// Read a provider API key from `TETON_PROVIDER_KEY` or, failing that, from the
/// caller's prompter. The key is handed straight to the keychain and never
/// written to a file.
///
/// The prompter is passed rather than made here (REQ-582 ADR-3): the shell's
/// `provider add` runs under a passive context whose prompter is a
/// [`StdinPrompter`] — what this function used to build for itself — and the
/// session's `/provider add` runs under the session's own dialogue prompter, so
/// the question is asked where the session is having its conversation. Which
/// *kind* of question it is stays [`prompt_for_secret`]'s decision (`ask_secret`,
/// echo-off) and is therefore the same on both surfaces.
///
/// # Errors
///
/// Exactly one: nothing was typed — an empty answer or EOF. Callers that must
/// not end on it map that single error to their own refusal
/// ([`ProviderAddRefusal::NoKey`]).
fn read_secret(id: &str, prompter: &mut dyn Prompter) -> anyhow::Result<String> {
    if let Ok(key) = std::env::var("TETON_PROVIDER_KEY") {
        let key = key.trim().to_owned();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    prompt_for_secret(id, prompter)
}

/// Ask for a provider API key through the **hiding** prompt (REQ-572 AC-5).
///
/// `ask_secret`, not `ask`: this is the same class of value `/web setup`
/// collects — a credential typed at a terminal — and `teton provider add` was
/// painting it into the user's scrollback and into any recording of the session.
/// One prompt kind for one kind of value.
///
/// Split out of [`read_secret`] so that choice is assertable without a terminal
/// and without the environment: the env-var shortcut stays above, and what is
/// left here is which seam the question goes through.
fn prompt_for_secret(id: &str, prompter: &mut dyn Prompter) -> anyhow::Result<String> {
    match prompter.ask_secret(&format!(
        "API key for `{id}` (not shown; stored only in the keychain): "
    )) {
        Some(key) if !key.trim().is_empty() => Ok(key.trim().to_owned()),
        _ => anyhow::bail!("no API key provided; set TETON_PROVIDER_KEY or enter the key"),
    }
}

/// Render a provider list to a surface.
///
/// The endpoint column goes through [`escaped_endpoint`] like every other line
/// the CLI prints an endpoint into (REQ-578 verify). This listing sits directly
/// under doctor's advisory, which redacts — so leaving this one raw meant doctor
/// printed a password in full and then masked the same password one line later.
/// The stored value is unchanged; `teton doctor` is a diagnostic surface whose
/// output people paste into issues.
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
        let endpoint = provider
            .endpoint
            .as_deref()
            .map_or_else(|| "(local)".to_owned(), escaped_endpoint);
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
///
/// `pub(crate)` so [`provider_test_ui`]'s preview reads the same four spellings
/// `teton provider list` prints. A second table would be the mirrored-predicate
/// shape LESSON-528 is about — identical today, and identical only until one of
/// them is edited.
pub(crate) fn kind_label(kind: ProviderKind) -> &'static str {
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
    use teton_core::ANTHROPIC_DEFAULT_ENDPOINT;

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

    /// REQ-583 BR-6 / AC-12: `--cwd` parses as a top-level flag, and its value
    /// obeys teton-core's grammar table — the **same rows** the `/cd` test in
    /// `slash.rs` and teton-core's own test iterate, so `--cwd` and `/cd`
    /// accept and reject the same spellings by construction.
    #[test]
    fn cwd_flag_parses_and_resolves_by_the_shared_grammar_table() {
        use teton_core::session_root::{
            CWD_ARGUMENT_GRAMMAR, CWD_GRAMMAR_HOME, CWD_GRAMMAR_SHELL_CWD,
        };
        let shell_cwd = Path::new(CWD_GRAMMAR_SHELL_CWD);
        let home = Path::new(CWD_GRAMMAR_HOME);

        // Absent: the shell's directory, exactly as before the flag existed.
        assert!(parse(&["teton"]).cwd.is_none());
        assert_eq!(
            session_root_for(None, Some(shell_cwd), Some(home)),
            Ok(Some(shell_cwd.to_path_buf()))
        );
        assert_eq!(session_root_for(None, None, Some(home)), Ok(None));

        for row in CWD_ARGUMENT_GRAMMAR {
            let cli = parse(&["teton", "--cwd", row.raw]);
            assert_eq!(
                cli.cwd.as_deref(),
                Some(row.raw),
                "clap must carry the value verbatim"
            );
            let got = session_root_for(cli.cwd.as_deref(), Some(shell_cwd), Some(home));
            match row.expect {
                Ok(path) => assert_eq!(
                    got,
                    Ok(Some(PathBuf::from(path))),
                    "--cwd {:?} must resolve to {path}",
                    row.raw
                ),
                Err(fragment) => {
                    let err = got.expect_err("the row must be refused");
                    assert!(
                        err.to_string().contains(fragment),
                        "--cwd {:?}: {err} must mention {fragment:?}",
                        row.raw
                    );
                }
            }
        }
        // AC-12's named spellings, read back through the parser as a script
        // would type them: relative, `~/x`, absolute, and empty → refused.
        assert_eq!(
            session_root_for(Some("rel"), Some(shell_cwd), Some(home)),
            Ok(Some(PathBuf::from("/work/here/rel")))
        );
        assert_eq!(
            session_root_for(Some("~/x"), Some(shell_cwd), Some(home)),
            Ok(Some(PathBuf::from("/home/u/x")))
        );
        assert_eq!(
            session_root_for(Some("/abs"), Some(shell_cwd), Some(home)),
            Ok(Some(PathBuf::from("/abs")))
        );
        assert_eq!(
            session_root_for(Some(""), Some(shell_cwd), Some(home)),
            Err(CwdArgError::Empty)
        );
        // No shell directory: an absolute argument still resolves, a relative
        // one is refused rather than guessed at.
        assert_eq!(
            session_root_for(Some("/abs"), None, Some(home)),
            Ok(Some(PathBuf::from("/abs")))
        );
        assert!(matches!(
            session_root_for(Some("rel"), None, Some(home)),
            Err(CwdArgError::NotAbsolute(_))
        ));
    }

    /// `--cwd` is a flag of the session (and of the one subcommand that opens a
    /// session, `provider test`), so it sits **before** a subcommand and is not
    /// `global`: after one it is an error, the way any unknown flag is. The
    /// two-way pin in `slash.rs` (`LEADING_GLOBAL_FLAGS`) is what this keeps
    /// honest — a global `--cwd` would be stepped over and dropped there.
    #[test]
    fn cwd_flag_is_not_global() {
        let cli = parse(&["teton", "--cwd", "/x", "provider", "test", "kimi"]);
        assert_eq!(cli.cwd.as_deref(), Some("/x"));
        assert!(matches!(
            cli.command,
            Some(Command::Provider {
                action: ProviderAction::Test { .. }
            })
        ));
        assert!(
            Cli::try_parse_from(["teton", "doctor", "--cwd", "/x"]).is_err(),
            "--cwd after a subcommand must be a parse error, not a global flag"
        );
        assert!(Cli::try_parse_from(["teton", "provider", "test", "kimi", "--cwd", "/x"]).is_err());
        use clap::CommandFactory;
        let root = Cli::command();
        let cwd = root
            .get_arguments()
            .find(|arg| arg.get_id() == "cwd")
            .expect("the --cwd argument");
        assert!(!cwd.is_global_set(), "--cwd must not be global");
        assert_eq!(cwd.get_value_names().map(|names| names.len()), Some(1));
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
            effort: None,
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
            // REQ-572: no capability answer in this fixture, which is what a
            // daemon predating the field sends. The renderer's handling of a
            // state that *is* present belongs with the surface that draws it
            // (TASK-131/132), not with this routing-table fixture.
            web_capability: None,
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

    /// REQ-572 AC-5, at the other place a credential is typed: `teton provider
    /// add` asks through the **hiding** prompt.
    ///
    /// The same value, the same terminal, the same scrollback — a key echoed
    /// here is as leaked as a key echoed in `/web setup`, and this command
    /// predates the hiding path, so it was still using the echoing one. A
    /// scripted prompter has no echo to check; what it can prove is which seam
    /// the question went through, which is the thing that silently regresses.
    #[test]
    fn a_provider_key_is_asked_for_through_the_hiding_path() {
        let mut prompter = ScriptedPrompter::new(&["sk-provider-secret"]);
        let key = prompt_for_secret("anthropic", &mut prompter).unwrap();

        assert_eq!(key, "sk-provider-secret");
        assert_eq!(
            prompter.secrets.len(),
            1,
            "the key question must go through `ask_secret`, not `ask`"
        );
        assert!(
            prompter.secrets[0].contains("anthropic") && prompter.secrets[0].contains("not shown"),
            "the prompt names the provider and says the typing is hidden: {:?}",
            prompter.secrets[0]
        );

        // An empty answer is refused rather than stored as a credential.
        let mut blank = ScriptedPrompter::new(&["   "]);
        assert!(prompt_for_secret("anthropic", &mut blank).is_err());
        // As is EOF.
        let mut eof = ScriptedPrompter::new(&[]);
        assert!(prompt_for_secret("anthropic", &mut eof).is_err());
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

    // -----------------------------------------------------------------------
    // REQ-582: the shared bodies the session's mirrored rows call.
    //
    // What these pin is the part of the split a byte-diff cannot see: the
    // sentences that stopped being `bail!`s (ADR-3), the seam a credential
    // question goes through when the caller supplies the prompter, and the one
    // line `/doctor` is allowed to differ from `teton doctor` in (ADR-5). The
    // parity itself is `cli_e2e`'s, which drives the real binary.
    // -----------------------------------------------------------------------

    /// ADR-3: the three refusals kept their exact sentences on the way out of
    /// `bail!`.
    ///
    /// A shell still exits non-zero with these words — `run_provider_add` maps
    /// the value straight back to `anyhow::bail!("{refusal}")` — and the session
    /// renders the same words as one Error line. The e2e suite asserts the exit
    /// status and a fragment of two of them against the real binary; what is
    /// worth pinning here is the *whole* sentence, because a refusal is the only
    /// output these paths produce and a paraphrase would pass every other test.
    #[test]
    fn the_three_provider_add_refusals_keep_their_sentences() {
        assert_eq!(
            ProviderAddRefusal::RemoteWithoutModel {
                id: "kimi".to_owned()
            }
            .to_string(),
            "provider `kimi` is a remote provider and must declare the model it calls: pass \
             `--model <name>` (e.g. `--model claude-opus-5`). The model is never inferred from \
             the provider id."
        );
        assert_eq!(
            ProviderAddRefusal::DuplicateId {
                id: "kimi".to_owned()
            }
            .to_string(),
            "provider `kimi` is already registered. Ids are unique — pick a different one (e.g. \
             `kimi-2`) if you want a second provider, which is how one vendor serves two models. \
             Nothing was changed and no credential was read."
        );
        assert_eq!(
            ProviderAddRefusal::NoKey.to_string(),
            "no API key provided; set TETON_PROVIDER_KEY or enter the key"
        );
    }

    /// REQ-557 BR-1's predicate, now that both surfaces ask it: only a remote
    /// kind owes a model, and only a blank one is missing.
    #[test]
    fn only_a_remote_provider_with_no_model_is_refused_for_it() {
        let refused = |kind, model| {
            remote_provider_needs_model("kimi", kind, model)
                .map(|refusal| refusal.to_string())
                .is_some()
        };
        assert!(refused(ProviderKind::OpenaiCompatible, None));
        // A model that is only whitespace is no model — the same reading the
        // shipped check made.
        assert!(refused(ProviderKind::Anthropic, Some("   ")));
        assert!(refused(ProviderKind::Custom, Some("")));
        assert!(!refused(ProviderKind::OpenaiCompatible, Some("kimi-k3")));
        // The local tier's model belongs to the REQ-547 consent flow, so a local
        // provider owes this command nothing.
        assert!(!refused(ProviderKind::Local, None));
    }

    /// ADR-3: `read_secret` asks through the **caller's** prompter, and asks the
    /// hiding question.
    ///
    /// The session path passes `ctx.prompter`, so this is the assertion that
    /// `/provider add` will collect a key echo-off through the session's own
    /// dialogue prompter rather than through a `StdinPrompter` this function
    /// used to build for itself (REQ-549 BR-5, REQ-572 AC-5).
    #[test]
    fn read_secret_asks_the_callers_prompter_and_never_echoes() {
        // The env shortcut is what the e2e suite removes from every child's
        // environment; a developer who exports it must still run the test CI
        // runs, so the prompter leg is only meaningful with it unset.
        if std::env::var("TETON_PROVIDER_KEY").is_ok_and(|key| !key.trim().is_empty()) {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let mut prompter = ScriptedPrompter::new(&["sk-session-secret"]);
        let key = read_secret("kimi", &mut prompter).unwrap();

        assert_eq!(key, "sk-session-secret");
        assert_eq!(
            prompter.secrets.len(),
            1,
            "the key question must go through `ask_secret`, not `ask`"
        );
        assert_eq!(
            prompter.questions.len(),
            1,
            "exactly one question, and it was the hiding one: {:?}",
            prompter.questions
        );
        assert_eq!(prompter.questions[0], prompter.secrets[0]);
        // EOF at the prompt is the refusal `provider_add_on` turns into
        // `ProviderAddRefusal::NoKey` rather than an `Err` that ends a session.
        let mut eof = ScriptedPrompter::new(&[]);
        assert!(read_secret("kimi", &mut eof).is_err());
    }

    fn doctor_paths() -> DaemonPaths {
        DaemonPaths {
            socket: std::path::PathBuf::from("/tmp/teton-test/teton.sock"),
            lock: std::path::PathBuf::from("/tmp/teton-test/teton.lock"),
            log: std::path::PathBuf::from("/tmp/teton-test/teton.log"),
        }
    }

    fn handshook() -> HandshakeResult {
        HandshakeResult {
            protocol_version: teton_protocol::ProtocolVersion(2),
            daemon_name: "teton-code".to_owned(),
            daemon_version: "0.1.20".to_owned(),
            capabilities: Vec::new(),
        }
    }

    /// ADR-5 / BR-7: the session's `/doctor` differs from `teton doctor` in
    /// **one** line, and it is the one that says which connection is being
    /// reported on.
    ///
    /// Driven over `doctor_preamble` because that is the whole surface the
    /// attach is in scope for — after it the report has no `attach` to consult,
    /// so the config listing, the base-URL advice and the two closing notices
    /// are the same code by construction rather than by assertion. What a test
    /// can still get wrong is the header drifting between the arms, and that is
    /// what this compares.
    #[test]
    fn the_session_doctor_differs_from_the_shell_one_in_exactly_the_daemon_line() {
        let paths = doctor_paths();
        let mut fresh = RecordingSurface::new();
        doctor_preamble(&paths, &DoctorAttach::Fresh(handshook()), &mut fresh);
        let mut session = RecordingSurface::new();
        // A name that is **not** the `daemon_line` fallback literal (verify
        // T13): a mutation that ignored the field and printed the fallback would
        // otherwise pass this test by coincidence.
        doctor_preamble(
            &paths,
            &DoctorAttach::Session {
                daemon_name: Some("teton-code-test".to_owned()),
                daemon_version: Some("0.1.20".to_owned()),
            },
            &mut session,
        );

        let fresh_lines = fresh.lines_of(LineKind::Info);
        let session_lines = session.lines_of(LineKind::Info);
        assert_eq!(
            fresh.calls.len(),
            session.calls.len(),
            "the two arms render the same number of lines"
        );
        let differing: Vec<usize> = fresh_lines
            .iter()
            .zip(&session_lines)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        assert_eq!(
            differing,
            vec![3],
            "only the daemon line may differ: shell {fresh_lines:?} vs session {session_lines:?}"
        );
        assert_eq!(
            fresh_lines[3],
            "daemon: running — teton-code 0.1.20 (protocol 2)"
        );
        assert_eq!(
            session_lines[3],
            "daemon: running — teton-code-test 0.1.20 (this session's connection)"
        );
        // The header is the shell's, byte for byte, on both paths — this is the
        // half of AC-1 the `/doctor` carve-out does not excuse.
        assert_eq!(fresh_lines[0], "teton doctor");
        assert!(fresh_lines[1].ends_with("teton.sock"));
        assert!(fresh_lines[2].ends_with("teton.lock"));
    }

    /// A `Session` attach made from a connection that never handshook still
    /// renders a report rather than panicking — the state is unreachable through
    /// `ensure_connected`, and an unreachable state is not a reason to lose the
    /// other fourteen lines of a diagnosis.
    #[test]
    fn a_session_attach_with_no_handshake_facts_still_names_the_connection() {
        let line = DoctorAttach::Session {
            daemon_name: None,
            daemon_version: None,
        }
        .daemon_line();
        assert!(line.contains("this session's connection"), "{line}");
        assert!(line.starts_with("daemon: running — teton-code"), "{line}");
    }

    /// **T1.** `doctor_report_on`'s two `config/get` failure arms, on both
    /// attach modes: a daemon too old for the method says so as a notice, and
    /// any other refusal is an error line — with the report's header, daemon
    /// line and trailer around them either way. The arms are the shell's own
    /// wording (BR-2), and until this test only the success arm had been driven
    /// through the session's `/doctor`.
    #[test]
    fn doctor_reports_a_config_query_the_daemon_refused_on_both_attach_modes() {
        let paths = doctor_paths();
        let too_old = RpcError {
            code: error_code::METHOD_NOT_FOUND,
            message: "no such method".to_owned(),
            data: None,
        };
        let refused = RpcError {
            code: error_code::INTERNAL_ERROR,
            message: "config is locked".to_owned(),
            data: None,
        };
        let attaches = || {
            [
                DoctorAttach::Fresh(handshook()),
                DoctorAttach::Session {
                    daemon_name: Some("teton-code-test".to_owned()),
                    daemon_version: Some("0.1.20".to_owned()),
                },
            ]
        };
        for attach in attaches() {
            let (mut conn, peer) = Connection::scripted_replies(vec![Err(too_old.clone())]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
                doctor_report_on(&paths, &mut conn, &mut ctx, &attach)
                    .expect("a daemon that answers is reported, not failed on");
            }
            assert_eq!(client::methods_written(&peer), vec!["config/get"]);
            assert!(
                surface
                    .lines_of(LineKind::Notice)
                    .iter()
                    .any(|line| line.contains("config: not exposed by this daemon build")),
                "{:?}",
                surface.calls
            );
            assert!(
                surface.lines_of(LineKind::Error).is_empty(),
                "{:?}",
                surface.calls
            );
            // The report is still whole around it: header first, trailer last.
            assert_eq!(surface.lines_of(LineKind::Info)[0], "teton doctor");
            assert!(
                surface
                    .lines_of(LineKind::Notice)
                    .last()
                    .is_some_and(|line| line.starts_with("providers:")),
                "{:?}",
                surface.calls
            );
            conn.assert_all_consumed();
        }
        for attach in attaches() {
            let (mut conn, peer) = Connection::scripted_replies(vec![Err(refused.clone())]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
                doctor_report_on(&paths, &mut conn, &mut ctx, &attach)
                    .expect("a daemon that answers is reported, not failed on");
            }
            assert_eq!(client::methods_written(&peer), vec!["config/get"]);
            assert_eq!(
                surface.lines_of(LineKind::Error),
                vec!["config query failed: config is locked"],
                "{:?}",
                surface.calls
            );
            assert!(
                surface
                    .lines_of(LineKind::Info)
                    .iter()
                    .any(|line| line.starts_with("daemon: running — ")),
                "{:?}",
                surface.calls
            );
            conn.assert_all_consumed();
        }
    }

    /// **m3 / T12.** The six arms of `run_mirrored_command` that no caller can
    /// reach — a `Command` that is not one of the ten mirrored rows — each
    /// render one line, send nothing, and never `Err` or panic. Driven by
    /// calling the dispatcher directly with hand-built commands, since that is
    /// the only way to reach them: the classifier refuses `uninstall` and the
    /// retired `policy set` before any row runs, and the four pre-REQ rows go
    /// through `run_cli_line` to their own handlers.
    #[test]
    fn a_command_that_is_not_a_mirrored_row_renders_one_line_and_runs_nothing() {
        let unreachable: Vec<(Command, &str)> = vec![
            (Command::Cost, "cost"),
            (Command::Effort { level: None }, "effort"),
            (
                Command::Model {
                    action: ModelAction::Set {
                        name: "qwen".to_owned(),
                    },
                },
                "model set",
            ),
            (
                Command::Provider {
                    action: ProviderAction::Test {
                        id: "kimi".to_owned(),
                    },
                },
                "provider test",
            ),
            (
                Command::Policy {
                    action: PolicyAction::Set { args: Vec::new() },
                },
                "policy set",
            ),
            (Command::Uninstall { keep_data: false }, "uninstall"),
        ];
        for (command, spelling) in unreachable {
            let (mut conn, peer) = Connection::scripted(&[]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&["y"]);
            {
                let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
                run_mirrored_command(command, &mut conn, &mut ctx)
                    .unwrap_or_else(|err| panic!("`teton {spelling}` failed: {err:#}"));
            }
            let lines = surface.lines_of(LineKind::Error);
            assert_eq!(
                lines,
                vec![not_a_mirrored_row(spelling)],
                "`teton {spelling}`: {:?}",
                surface.calls
            );
            assert_eq!(
                surface.calls.len(),
                1,
                "one line, and only one: {:?}",
                surface.calls
            );
            assert!(
                lines[0].contains(&format!("`teton {spelling}`")),
                "{}",
                lines[0]
            );
            assert!(lines[0].contains("/help"), "{}", lines[0]);
            // No "mirrored" in a user-facing sentence (verify m14).
            assert!(!lines[0].contains("mirrored"), "{}", lines[0]);
            assert!(
                client::methods_written(&peer).is_empty(),
                "`teton {spelling}` reached the daemon"
            );
            assert_eq!(prompter.asked, 0, "`teton {spelling}` asked a question");
        }
    }

    // -----------------------------------------------------------------------
    // REQ-582 verify M1/M4: `provider_add_on`, composed, against a keychain
    // double — the read → store → `config/set` path and the session's consent
    // before it, which the pty suite cannot walk without writing to a real
    // login keychain.
    // -----------------------------------------------------------------------

    /// The credential the composed tests type. Distinctive, so a sweep of the
    /// wire and the surface for it means something (LESSON-519).
    const TYPED_KEY: &str = "sk-composed-provider-add-9Qm2vX";

    /// The provider these tests register: id, kind, endpoint, model — the shape
    /// AC-3 names, with a full request URL so the seam stores it as typed.
    const ADD_ID: &str = "kimi";
    const ADD_ENDPOINT: &str = "https://api.moonshot.ai/v1/chat/completions";
    const ADD_MODEL: &str = "kimi-k3";

    /// A daemon that knows no provider named [`ADD_ID`] — the duplicate probe's
    /// answer that lets a registration proceed.
    fn no_such_provider() -> serde_json::Value {
        serde_json::to_value(teton_protocol::methods::ConfigGetResult::default())
            .expect("a config snapshot serializes")
    }

    /// `config/set`'s "applied" answer.
    fn applied() -> serde_json::Value {
        serde_json::to_value(ConfigSetResult { applied: true }).expect("a set result serializes")
    }

    /// Run `provider_add_on` for [`ADD_ID`] under the session's consent mode,
    /// with `answers` scripted on the session's prompter and `replies` scripted
    /// on the connection. Returns what was rendered, what was asked, and the
    /// frames that reached the socket; the keychain is the caller's, so the
    /// caller can read it afterwards.
    #[allow(clippy::type_complexity)]
    fn add_in_session(
        answers: &[&str],
        replies: Vec<Result<serde_json::Value, RpcError>>,
        assume_yes: bool,
        keychain: &MockKeychain,
    ) -> (
        Result<(), ProviderAddRefusal>,
        RecordingSurface,
        ScriptedPrompter,
        Vec<serde_json::Value>,
    ) {
        let (mut conn, peer) = Connection::scripted_replies(replies);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(answers);
        let outcome = {
            let mut ctx = UiContext {
                surface: &mut surface,
                state: &mut state,
                prompter: &mut prompter,
                answer_permissions: true,
                answer_model_proposals: true,
                auto_accept_model: assume_yes,
                typed_input: true,
                session_id: None,
            };
            provider_add_on(
                &mut conn,
                &mut ctx,
                ADD_ID,
                ProviderKind::OpenaiCompatible,
                Some(ADD_ENDPOINT.to_owned()),
                Some(ADD_MODEL.to_owned()),
                AddConsent::Session { assume_yes },
                keychain,
            )
            .expect("no transport failure was scripted")
        };
        conn.assert_all_consumed();
        (outcome, surface, prompter, client::requests_written(&peer))
    }

    /// Whether `TETON_PROVIDER_KEY` is exported in this process — in which case
    /// `read_secret` never reaches the prompter and every count of secret
    /// questions below would be off by one. The e2e suite removes it from every
    /// child's environment; a developer who exports it must still run the tests
    /// CI runs, so the tests that read a key return early rather than fail
    /// (the same guard `read_secret_asks_the_callers_prompter_and_never_echoes`
    /// takes) — saying so on stderr first, so a green run with the key exported
    /// reads as the skip it is under `--nocapture` rather than as proof.
    fn provider_key_exported() -> bool {
        std::env::var("TETON_PROVIDER_KEY").is_ok_and(|key| !key.trim().is_empty())
    }

    /// Every byte the socket saw, as one string — the haystack for "the key
    /// never crossed the wire".
    fn wire_text(frames: &[serde_json::Value]) -> String {
        frames
            .iter()
            .map(|frame| frame.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **M1.** The session confirms, default-no, **before** the key is read.
    /// "n", an empty answer, and a second pasted command line all decline:
    /// `ask_secret` is never called, the keychain is untouched, no `config/set`
    /// goes on the socket, and the one line says exactly that.
    #[test]
    fn a_declined_session_provider_add_reads_no_key_and_stores_nothing() {
        for (case, answer) in [
            ("an explicit no", "n"),
            ("an empty answer", ""),
            ("the second line of a paste", "teton policy show"),
        ] {
            let kc = MockKeychain::new();
            let (outcome, surface, prompter, frames) =
                add_in_session(&[answer], vec![Ok(no_such_provider())], false, &kc);

            assert!(
                outcome.is_ok(),
                "{case}: a decline is not a refusal: {outcome:?}"
            );
            // The confirmation was asked, plainly, once — and nothing after it.
            assert_eq!(
                prompter.asked, 1,
                "{case}: one question, the confirmation: {:?}",
                prompter.questions
            );
            assert!(
                prompter.secrets.is_empty(),
                "{case}: `ask_secret` was called after a decline: {:?}",
                prompter.secrets
            );
            let question = &prompter.questions[0];
            assert!(
                question.ends_with("[y/N] "),
                "{case}: default-no: {question}"
            );
            for named in [ADD_ID, "openai-compatible", ADD_MODEL, ADD_ENDPOINT] {
                assert!(
                    question.contains(named),
                    "{case}: the question must name `{named}`: {question}"
                );
            }
            // Nothing stored, nothing registered: the probe is the only frame.
            assert!(kc.is_empty(), "{case}: the keychain gained an entry");
            assert_eq!(
                frames.len(),
                1,
                "{case}: only the duplicate probe may reach the socket: {frames:?}"
            );
            assert_eq!(frames[0]["method"], "config/get");
            // One Info line, saying what did not happen; no error.
            assert_eq!(
                surface.lines_of(LineKind::Info),
                vec![provider_add_declined_line(ADD_ID)],
                "{case}: {:?}",
                surface.calls
            );
            assert!(
                surface.lines_of(LineKind::Info)[0].contains("nothing registered")
                    && surface.lines_of(LineKind::Info)[0].contains("no key read"),
                "{case}: {:?}",
                surface.calls
            );
            assert!(
                surface.lines_of(LineKind::Error).is_empty(),
                "{case}: {:?}",
                surface.calls
            );
        }
    }

    /// **M1's other half, and M4.** A "y" proceeds: the key is read through the
    /// hiding prompt, reaches the mock under the account the shell path uses,
    /// the `config/set` on the socket carries `keychain://teton/<id>` and no raw
    /// key, and neither the wire nor any surface line ever contains the key.
    #[test]
    fn a_confirmed_session_provider_add_stores_the_key_and_registers_by_reference() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::new();
        let (outcome, surface, prompter, frames) = add_in_session(
            &["y", TYPED_KEY],
            vec![Ok(no_such_provider()), Ok(applied())],
            false,
            &kc,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        // The confirmation first, plainly; the key second, hidden.
        assert_eq!(prompter.asked, 2, "{:?}", prompter.questions);
        assert!(prompter.questions[0].contains("[y/N]"));
        assert_eq!(prompter.secrets, vec![prompter.questions[1].clone()]);
        assert!(prompter.secrets[0].contains("API key for `kimi`"));

        // The key reached the mock, under the shell path's account (the id).
        assert_eq!(kc.stored_secret(ADD_ID).as_deref(), Some(TYPED_KEY));
        assert!(kc.deletes().is_empty(), "nothing was taken back out");

        // The wire: the probe, then the registration — with the reference and
        // never the key.
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0]["method"], "config/get");
        assert_eq!(frames[1]["method"], "config/set");
        let params = frames[1]["params"].to_string();
        assert!(
            params.contains(&format!("keychain://teton/{ADD_ID}")),
            "the registration must carry the keychain reference: {params}"
        );
        assert!(
            params.contains(ADD_ENDPOINT) && params.contains(ADD_MODEL),
            "the registration must carry the settled endpoint and model: {params}"
        );
        assert!(
            !wire_text(&frames).contains(TYPED_KEY),
            "the key crossed the wire: {}",
            wire_text(&frames)
        );
        // And no surface line — question, echo, report — carries it either.
        for call in &surface.calls {
            let text = format!("{call:?}");
            assert!(
                !text.contains(TYPED_KEY),
                "the key reached the surface: {text}"
            );
        }
        for question in &prompter.questions {
            assert!(
                !question.contains(TYPED_KEY),
                "the key was asked back: {question}"
            );
        }
        assert!(
            surface
                .lines_of(LineKind::Info)
                .iter()
                .any(|line| line.contains("registered") && line.contains("keychain")),
            "the success line must say the key went to the keychain: {:?}",
            surface.calls
        );
    }

    /// **M1.** The session's own `--yes` pre-answers the confirmation and
    /// consumes no input line: the key is the first and only thing asked for.
    #[test]
    fn the_sessions_yes_pre_answers_the_provider_add_confirmation() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::new();
        let (outcome, _surface, prompter, frames) = add_in_session(
            &[TYPED_KEY],
            vec![Ok(no_such_provider()), Ok(applied())],
            true,
            &kc,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(prompter.asked, 1, "{:?}", prompter.questions);
        assert_eq!(prompter.secrets.len(), 1);
        assert_eq!(kc.stored_secret(ADD_ID).as_deref(), Some(TYPED_KEY));
        assert_eq!(frames.len(), 2);
        assert!(!wire_text(&frames).contains(TYPED_KEY));
    }

    /// **M4.** A `config/set` the daemon refuses takes the stored key back out
    /// of the mock (BUG-171's undo through the composed flow, `PriorKey`
    /// against a double), and says so.
    #[test]
    fn a_refused_session_registration_takes_its_stored_key_back_out() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::new();
        let (outcome, surface, _prompter, frames) = add_in_session(
            &["y", TYPED_KEY],
            vec![
                Ok(no_such_provider()),
                Err(RpcError::new(
                    error_code::INVALID_PARAMS,
                    "provider `kimi` was refused by the daemon",
                )),
            ],
            false,
            &kc,
        );

        assert!(
            outcome.is_ok(),
            "a refused registration is reported, not an Err: {outcome:?}"
        );
        // Stored, then deleted: the record says the undo ran, and the store is
        // empty afterwards.
        assert_eq!(kc.deletes(), vec![ADD_ID.to_owned()]);
        assert!(
            kc.is_empty(),
            "the refused registration left its key behind"
        );
        assert_eq!(frames.len(), 2);
        assert!(!wire_text(&frames).contains(TYPED_KEY));
        assert!(
            surface
                .lines_of(LineKind::Error)
                .iter()
                .any(|line| line.contains("registration rejected")),
            "{:?}",
            surface.calls
        );
        assert!(
            surface
                .lines_of(LineKind::Notice)
                .iter()
                .any(|line| line.contains("has been removed from your keychain")),
            "{:?}",
            surface.calls
        );
        for call in &surface.calls {
            assert!(!format!("{call:?}").contains(TYPED_KEY));
        }
    }

    /// **M4, the displaced-credential arm.** When the account already held a
    /// key, a refused registration puts *that* key back rather than deleting.
    #[test]
    fn a_refused_session_registration_restores_the_key_it_displaced() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::new();
        kc.store(ADD_ID, "sk-the-old-one").expect("seed the mock");
        let (outcome, surface, _prompter, _frames) = add_in_session(
            &["y", TYPED_KEY],
            vec![
                Ok(no_such_provider()),
                Err(RpcError::new(error_code::INVALID_PARAMS, "refused")),
            ],
            false,
            &kc,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(kc.stored_secret(ADD_ID).as_deref(), Some("sk-the-old-one"));
        assert!(kc.deletes().is_empty(), "a restore is not a delete");
        assert!(
            surface
                .lines_of(LineKind::Notice)
                .iter()
                .any(|line| line.contains("put back to the credential it held")),
            "{:?}",
            surface.calls
        );
    }

    /// **m7.** A keychain that will not store is a rendered refusal, not an
    /// `Err`: nothing is registered, and the sentence is the store's own.
    #[test]
    fn a_keychain_that_will_not_store_is_a_refusal_and_registers_nothing() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::unavailable();
        let (outcome, _surface, prompter, frames) =
            add_in_session(&["y", TYPED_KEY], vec![Ok(no_such_provider())], false, &kc);
        let refusal = outcome.expect_err("a store failure is a refusal");
        assert!(
            matches!(refusal, ProviderAddRefusal::KeychainStore(_)),
            "{refusal:?}"
        );
        assert!(
            refusal.to_string().contains("no OS keychain is available"),
            "{refusal}"
        );
        // The key was read (the user consented) but nothing was sent: the
        // probe is the only frame.
        assert_eq!(prompter.secrets.len(), 1);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert!(!wire_text(&frames).contains(TYPED_KEY));
    }

    /// The shell asks nothing (`AddConsent::Shell`): its command line was the
    /// consent, and its first question is the key.
    #[test]
    fn the_shell_consent_mode_asks_no_confirmation() {
        if provider_key_exported() {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
            return;
        }
        let kc = MockKeychain::new();
        let (mut conn, peer) = Connection::scripted(&[no_such_provider(), applied()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[TYPED_KEY]);
        {
            let mut ctx = passive_ctx(&mut surface, &mut state, &mut prompter);
            provider_add_on(
                &mut conn,
                &mut ctx,
                ADD_ID,
                ProviderKind::OpenaiCompatible,
                Some(ADD_ENDPOINT.to_owned()),
                Some(ADD_MODEL.to_owned()),
                AddConsent::Shell,
                &kc,
            )
            .expect("no transport failure")
            .expect("registered");
        }
        assert_eq!(prompter.asked, 1, "{:?}", prompter.questions);
        assert_eq!(prompter.secrets.len(), 1);
        assert_eq!(kc.stored_secret(ADD_ID).as_deref(), Some(TYPED_KEY));
        assert_eq!(
            client::methods_written(&peer),
            vec!["config/get", "config/set"]
        );
        conn.assert_all_consumed();
    }

    // -----------------------------------------------------------------------
    // REQ-578: what `provider add` decides to store, and when it says so.
    //
    // The composition *rule* is tabled where it lives
    // (`teton_core::endpoint_composition`); what these drive is the CLI's half
    // of it — the echo (BR-4), the Anthropic default arriving through the same
    // seam (BR-3), and the refusal that has to land before a credential is
    // typed (BR-5). AC-1..AC-5 through a real daemon are the e2e's.
    // -----------------------------------------------------------------------

    /// Run the composition step the way `run_provider_add` runs it: what it
    /// settled on, and everything it said while doing so.
    fn settled(kind: ProviderKind, endpoint: Option<&str>) -> (Option<String>, RecordingSurface) {
        let mut surface = RecordingSurface::new();
        let stored = settle_endpoint("kimi", kind, endpoint.map(str::to_owned), &mut surface)
            .expect("a structurally complete registration settles");
        (stored, surface)
    }

    /// AC-1's input: the base URL Moonshot documents becomes the request URL
    /// Teton POSTs, and the user is told the full form.
    #[test]
    fn a_pasted_base_url_is_composed_and_the_stored_form_is_echoed() {
        let (stored, surface) = settled(
            ProviderKind::OpenaiCompatible,
            Some("https://api.moonshot.ai/v1"),
        );

        assert_eq!(
            stored.as_deref(),
            Some("https://api.moonshot.ai/v1/chat/completions"),
            "the persisted value is always the absolute request URL (BR-1)"
        );
        assert!(
            surface.any_line_contains(
                LineKind::Info,
                "https://api.moonshot.ai/v1/chat/completions"
            ),
            "a composed endpoint has to be said out loud, in full: {:?}",
            surface.lines_of(LineKind::Info)
        );
    }

    /// The echo's load-bearing case (ADR-2's recorded known limit): a bare
    /// *origin* for a `/v1`-family vendor composes to a URL that vendor does
    /// **not** serve — `https://api.openai.com/chat/completions` is a 404.
    ///
    /// That is BR-2 as specified rather than a defect: the rule is per *kind*,
    /// and the kind cannot know whose base URL carries a version segment
    /// (OpenAI's does, DeepSeek's does not). What makes the limit survivable is
    /// that the composed URL is shown before a credential is typed — so the
    /// visibility is itself a claim, and this is the test of it. If a future
    /// edit ever narrows the echo to "only when we are confident", this fails,
    /// which is the intent.
    #[test]
    fn a_bare_origin_is_echoed_precisely_because_the_composed_url_may_be_wrong() {
        let (stored, surface) = settled(
            ProviderKind::OpenaiCompatible,
            Some("https://api.openai.com"),
        );

        assert_eq!(
            stored.as_deref(),
            Some("https://api.openai.com/chat/completions")
        );
        assert!(
            surface.any_line_contains(LineKind::Info, "https://api.openai.com/chat/completions"),
            "the one mitigation for the known limit is that the user sees this URL while a \
             credential is still untyped: {:?}",
            surface.lines_of(LineKind::Info)
        );
    }

    /// BR-3/AC-3: `--kind anthropic` with no `--endpoint` registers the official
    /// Messages URL, written explicitly and echoed like any other composed value.
    #[test]
    fn the_anthropic_default_endpoint_is_stored_and_echoed() {
        let (stored, surface) = settled(ProviderKind::Anthropic, None);

        assert_eq!(stored.as_deref(), Some(ANTHROPIC_DEFAULT_ENDPOINT));
        assert!(
            surface.any_line_contains(LineKind::Info, ANTHROPIC_DEFAULT_ENDPOINT),
            "a default the user did not type is exactly the case they need told about: {:?}",
            surface.lines_of(LineKind::Info)
        );
    }

    /// AC-2 and AC-4: an endpoint stored exactly as typed produces no line at
    /// all — every previously documented full-URL command reads byte-identically
    /// to how it read before this REQ (BR-7), and a gateway's custom path is
    /// neither rewritten nor remarked upon.
    #[test]
    fn an_endpoint_stored_as_typed_says_nothing() {
        for (kind, endpoint) in [
            (
                ProviderKind::OpenaiCompatible,
                "https://api.moonshot.ai/v1/chat/completions",
            ),
            (ProviderKind::Anthropic, ANTHROPIC_DEFAULT_ENDPOINT),
            // AC-4: an explicit path is somebody's deliberate address.
            (
                ProviderKind::OpenaiCompatible,
                "https://gw.example.com/llm/proxy",
            ),
            (ProviderKind::Anthropic, "https://gw.example.com/llm/proxy"),
        ] {
            let (stored, surface) = settled(kind, Some(endpoint));
            assert_eq!(stored.as_deref(), Some(endpoint));
            assert!(
                surface.calls.is_empty(),
                "{kind:?} with `{endpoint}` stored it verbatim and still said something: {:?}",
                surface.calls
            );
        }
    }

    /// BR-5, at the seam a unit test can reach: the stored endpoint is told
    /// before the key is asked for.
    ///
    /// The two halves are driven in the order [`run_provider_add`] calls them —
    /// [`settle_endpoint`], whose `?` gates everything after it, then the
    /// credential read — and what is pinned is that the echo is already on the
    /// surface at the moment the hiding prompt is put, and that the prompt adds
    /// nothing to the surface that could have preceded it. The whole-command
    /// form of this claim (real argv, real daemon) is AC-3's e2e.
    #[test]
    fn the_stored_endpoint_is_told_before_the_key_is_asked_for() {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["sk-typed-after-the-echo"]);

        let stored =
            settle_endpoint("claude", ProviderKind::Anthropic, None, &mut surface).unwrap();
        assert_eq!(stored.as_deref(), Some(ANTHROPIC_DEFAULT_ENDPOINT));

        assert!(
            prompter.secrets.is_empty(),
            "nothing may have been asked yet — this is the step that decides whether the \
             registration is worth typing a key into"
        );
        assert!(
            surface.any_line_contains(LineKind::Info, ANTHROPIC_DEFAULT_ENDPOINT),
            "and the user has already been told what will be called: {:?}",
            surface.lines_of(LineKind::Info)
        );
        let said_before_the_prompt = surface.calls.len();

        let key = prompt_for_secret("claude", &mut prompter).unwrap();

        assert_eq!(key, "sk-typed-after-the-echo");
        assert_eq!(prompter.secrets.len(), 1);
        assert_eq!(
            surface.calls.len(),
            said_before_the_prompt,
            "the credential prompt writes to the prompter, not the surface — so the echo \
             recorded above is unambiguously the earlier of the two"
        );
    }

    /// BR-5's other half: a registration that cannot work is refused with the
    /// credential still untyped, and the message names the flag that fixes it.
    ///
    /// This is the BUG-170 sequence, inverted. The predicate is the daemon
    /// validator's own (`is_remote && endpoint.trim().is_empty()`), so no new
    /// fatal class is introduced (BR-6) — it is the *same* refusal, moved in
    /// front of the prompt. `run_provider_add` reaches `read_secret` only past
    /// this `?`, so an `Err` here is a refusal with nothing asked and nothing
    /// stored.
    #[test]
    fn a_remote_registration_with_no_endpoint_refuses_before_any_prompt() {
        for endpoint in [None, Some("   ")] {
            let mut surface = RecordingSurface::new();
            let refused = settle_endpoint(
                "gw",
                ProviderKind::OpenaiCompatible,
                endpoint.map(str::to_owned),
                &mut surface,
            )
            .expect_err("an openai-compatible provider has no host to default to (BR-3)");

            let message = refused.to_string();
            assert!(
                message.contains("--endpoint"),
                "the refusal must name the flag: {message}"
            );
            assert!(
                message.contains("no credential was read"),
                "and say what it did not do with the user's key: {message}"
            );
            assert!(
                surface.calls.is_empty(),
                "a registration that is not happening has no stored endpoint to echo: {:?}",
                surface.calls
            );
        }

        // The on-device tier has no endpoint and never did — the refusal is
        // about remote kinds only, exactly as `Config::validate`'s is.
        let mut surface = RecordingSurface::new();
        let stored = settle_endpoint("local", ProviderKind::Local, None, &mut surface).unwrap();
        assert_eq!(stored, None);
        assert!(surface.calls.is_empty());
    }

    /// A `Local` registration says nothing about its endpoint, even when the
    /// stored value differs from what was typed (REQ-578 verify).
    ///
    /// Normalization trims, so a padded local endpoint is `changed` — and the
    /// echo's sentence is "that exact URL is what Teton will POST", which is
    /// false for a tier that POSTs nowhere. The trim still happens; only the
    /// claim about it is withheld.
    #[test]
    fn a_local_registration_is_never_told_what_teton_will_post() {
        let (stored, surface) = settled(ProviderKind::Local, Some("  http://127.0.0.1:8080  "));
        assert_eq!(
            stored.as_deref(),
            Some("http://127.0.0.1:8080"),
            "the value is still normalized — this is about the sentence, not the storage"
        );
        assert!(
            surface.calls.is_empty(),
            "the on-device tier POSTs nothing, so there is no URL to promise: {:?}",
            surface.calls
        );
    }

    /// The wire kind and the composition rule's kind are the same four kinds.
    ///
    /// A mapping that drifted would compose an Anthropic registration with the
    /// OpenAI-compatible path — a wrong URL written into a user's config by a
    /// typo no type checker can see. The mapping itself is now
    /// `teton_core`'s one `From` impl, shared with the daemon's `to_core_kind`
    /// (REQ-578 verify), and this is the CLI's own check that the conversion it
    /// reaches for is the right one.
    #[test]
    fn every_wire_kind_maps_to_its_own_composition_kind() {
        for (wire, core) in [
            (ProviderKind::Local, CoreProviderKind::Local),
            (
                ProviderKind::OpenaiCompatible,
                CoreProviderKind::OpenaiCompatible,
            ),
            (ProviderKind::Anthropic, CoreProviderKind::Anthropic),
            (ProviderKind::Custom, CoreProviderKind::Custom),
        ] {
            assert_eq!(
                CoreProviderKind::from(wire),
                core,
                "{wire:?} maps to the wrong kind"
            );
        }
    }

    /// `custom` names an operator's own adapter, whose protocol Teton does not
    /// know — so nothing is composed onto it, and what they typed is what they
    /// get. It is still remote, so it still owes an endpoint.
    ///
    /// And the refusal has to say *that*: the completion sentence every other
    /// remote kind gets ("your vendor's base URL is enough") is false here, and
    /// a user who follows it pastes a base URL, gets it stored verbatim, and
    /// lands on the 404 this REQ exists to prevent.
    #[test]
    fn a_custom_kind_is_never_composed_but_still_owes_an_endpoint() {
        let (stored, surface) = settled(ProviderKind::Custom, Some("https://gw.example.com/v1"));
        assert_eq!(stored.as_deref(), Some("https://gw.example.com/v1"));
        assert!(surface.calls.is_empty());

        let mut surface = RecordingSurface::new();
        let refused = settle_endpoint("gw", ProviderKind::Custom, None, &mut surface)
            .expect_err("a custom provider is remote and owes an endpoint");
        let message = refused.to_string();
        assert!(
            message.contains("--endpoint") && message.contains("full request URL"),
            "the refusal must name the flag and what to put after it: {message}"
        );
        assert!(
            !message.contains("Teton completes it"),
            "Teton completes nothing for a kind whose protocol it does not know, and a refusal \
             that promises otherwise sends the user back with a base URL: {message}"
        );

        // The kinds that *do* have a canonical path keep the sentence.
        let mut surface = RecordingSurface::new();
        let refused = settle_endpoint("gw", ProviderKind::OpenaiCompatible, None, &mut surface)
            .expect_err("an openai-compatible provider has no host to default to");
        assert!(
            refused.to_string().contains("Teton completes it"),
            "{refused}"
        );
    }

    /// An endpoint whose rendering and whose dialling would differ is refused
    /// outright, before anything else this function does (REQ-578 verify).
    ///
    /// TAB, LF and CR are deleted by URL parsers and drawn as spacing by a
    /// terminal, so an endpoint carrying one would be echoed as a string that is
    /// not the string Teton POSTs — with nothing on screen to say so. That
    /// defeats the single mitigation BR-4 offers for a composed URL, so the
    /// refusal comes first and, like every other refusal here, lands with the
    /// credential still untyped.
    #[test]
    fn an_endpoint_carrying_a_tab_or_a_newline_is_refused_before_any_prompt() {
        for supplied in [
            "https://api.moonshot.ai/v1\tchat",
            "https://api.moonshot.ai\n/v1",
            "https://api.moonshot.ai/v1\r",
            "\thttps://api.moonshot.ai/v1",
        ] {
            let mut surface = RecordingSurface::new();
            let refused = settle_endpoint(
                "kimi",
                ProviderKind::OpenaiCompatible,
                Some(supplied.to_owned()),
                &mut surface,
            )
            .expect_err("`{supplied}` cannot be echoed truthfully, so it is not stored");

            let message = refused.to_string();
            assert!(
                message.contains("tab, newline or carriage return"),
                "the refusal has to name what it found: {message}"
            );
            assert!(
                message.contains("no credential was read"),
                "and say what it did not do with the user's key: {message}"
            );
            assert!(
                surface.calls.is_empty(),
                "nothing may be echoed about a value that is not being stored: {:?}",
                surface.calls
            );
        }
    }

    /// A key about to be typed into an `http://` registration is a key about to
    /// cross the network in the clear, and the user is told so while it is still
    /// untyped (REQ-578 verify).
    ///
    /// Loopback is exempt and must stay exempt: a self-hosted Ollama on
    /// `http://localhost:11434` is the ordinary configuration this product
    /// supports, nothing leaves the machine, and a warning there is noise that
    /// teaches users to skip the one that matters.
    #[test]
    fn a_cleartext_endpoint_to_a_remote_host_is_warned_about_and_loopback_is_not() {
        let (stored, surface) =
            settled(ProviderKind::OpenaiCompatible, Some("http://192.0.2.1/v1"));
        assert_eq!(
            stored.as_deref(),
            Some("http://192.0.2.1/v1/chat/completions")
        );
        let warned = surface.lines_of(LineKind::Notice);
        assert_eq!(warned.len(), 1, "one warning, once: {warned:?}");
        assert!(
            warned[0].contains("192.0.2.1") && warned[0].contains("in the clear"),
            "the warning must name the host the key would reach: {warned:?}"
        );

        for silent in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:11434/v1",
            "http://[::1]:11434/v1",
            // TLS is the whole point of the check.
            "https://api.moonshot.ai/v1",
        ] {
            let (_, surface) = settled(ProviderKind::OpenaiCompatible, Some(silent));
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "`{silent}` exposes nothing and must not be warned about: {:?}",
                surface.lines_of(LineKind::Notice)
            );
        }

        // A local provider is never asked for a credential, so there is no
        // exposure to warn about even on a cleartext address.
        let (_, surface) = settled(ProviderKind::Local, Some("http://192.0.2.1/v1"));
        assert!(surface.lines_of(LineKind::Notice).is_empty());
    }

    /// The endpoint that was echoed is the endpoint that reaches the `config/set`
    /// payload (REQ-578 verify).
    ///
    /// The e2e suite stops one step short of the keychain, so it can show that
    /// the composed URL was *printed* and not that it was the value handed to
    /// the daemon. Those are two different strings until something compares
    /// them, and this is not hypothetical: a reviewer mutated the flow to pass
    /// the raw `--endpoint` argv into the registration while still echoing the
    /// composed one, and the entire suite stayed green — every AC asserted one
    /// half or the other, none asserted they were the same string.
    ///
    /// So this drives the two functions [`run_provider_add`] actually calls, in
    /// its order — [`settle_registration`], then [`registration_params`] — and
    /// reads the endpoint back out of the payload. The mutation above can no
    /// longer be *written* (settling consumes the argv value), and a mutation
    /// inside either function fails here.
    ///
    /// What stays a recorded known limit is the last hop: the whole CLI → RPC
    /// flow needs the real OS keychain, which has no test seam.
    #[test]
    fn the_endpoint_that_is_echoed_is_the_endpoint_that_is_registered() {
        for (kind, supplied, expected) in [
            (
                ProviderKind::OpenaiCompatible,
                Some("https://api.moonshot.ai/v1"),
                "https://api.moonshot.ai/v1/chat/completions",
            ),
            (ProviderKind::Anthropic, None, ANTHROPIC_DEFAULT_ENDPOINT),
            // A full URL settles to itself, so this row would pass even under
            // the mutation — it is here to show the comparison is not merely
            // "the composed form appears somewhere".
            (
                ProviderKind::OpenaiCompatible,
                Some("https://gw.example.com/llm/proxy"),
                "https://gw.example.com/llm/proxy",
            ),
        ] {
            let keychain = MockKeychain::new();
            let mut surface = RecordingSurface::new();

            let settled = settle_registration(
                "kimi",
                kind,
                supplied.map(str::to_owned),
                Some("a-model".to_owned()),
                &mut surface,
            )
            .expect("a structurally complete registration settles");
            let prepared =
                registration_params(&settled, &keychain, Some("sk-typed-after-the-echo"))
                    .expect("the mock keychain stores");

            let ConfigUpdate::RegisterProvider(config) = prepared.params.update else {
                panic!("`provider add` sends a provider registration and nothing else");
            };
            assert_eq!(
                config.endpoint.as_deref(),
                Some(expected),
                "{kind:?}: the payload must carry the settled endpoint, not a second reading of \
                 the argv"
            );
            assert_eq!(
                config.endpoint, settled.endpoint,
                "{kind:?}: and it must be the value the settle step returned, byte for byte"
            );
            assert_eq!(prepared.auth, config.auth_ref.unwrap_or_default());

            // The other half of the claim: the user saw this exact string. A
            // composed value is echoed; a verbatim one is not, and then there is
            // nothing to compare.
            if supplied != Some(expected) {
                assert!(
                    surface.any_line_contains(LineKind::Info, expected),
                    "and that value is the one the user was shown: {:?}",
                    surface.lines_of(LineKind::Info)
                );
            }
        }
    }

    /// The registration seam refuses every string a URL parser would read as an
    /// authority and a string-splitter would not (REQ-578 verify).
    ///
    /// These are not typos. `http:/host` and its three slash variants are
    /// accepted by the `url` crate as `http://host`, so a naive predicate reading
    /// the same string sees no scheme, no host, and nothing to warn about — while
    /// the request goes out to `host` in the clear. The backslash-in-authority
    /// shape is the same disagreement in the other direction: WHATWG says the
    /// host is `evil.example`, a `/?#` splitter says `127.0.0.1`, and the
    /// cleartext exemption would be handed to the wrong one of the two.
    ///
    /// The gate is `teton_core`'s [`is_absolute_http_url`], the same predicate a
    /// `[web]` search endpoint is held to — so the two surfaces cannot come to
    /// different conclusions about one string.
    #[test]
    fn a_url_shape_that_two_parsers_would_read_differently_is_refused_before_any_prompt() {
        let refused = [
            // The four slash variants `url` 2.5 resolves to `http://host`.
            "http:/api.moonshot.ai/v1",
            "http:\\\\api.moonshot.ai/v1",
            "http:/\\api.moonshot.ai/v1",
            "http:\\/api.moonshot.ai/v1",
            // Backslash in the authority: two readings, two different hosts.
            "https://evil.example\\@127.0.0.1/v1",
            // Hostless.
            "http:///v1",
            "https://",
            // No scheme at all — stored verbatim before this gate, and then
            // POSTed verbatim to nowhere.
            "api.moonshot.ai/v1",
            "localhost:11434",
            // A scheme Teton does not speak.
            "ftp://api.moonshot.ai/v1",
        ];

        for kind in [
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            for supplied in refused {
                let mut surface = RecordingSurface::new();
                let settled =
                    settle_endpoint("kimi", kind, Some(supplied.to_owned()), &mut surface);

                let refusal = settled.expect_err(&format!(
                    "{kind:?} must refuse `{supplied}` — a value two parsers read differently is \
                     one Teton cannot echo truthfully"
                ));
                let message = refusal.to_string();
                assert!(
                    message.contains("absolute `http://` or `https://` URL"),
                    "the refusal must name the shape it accepts: {message}"
                );
                assert!(
                    message.contains("no credential was read"),
                    "and say what it did not do with the user's key: {message}"
                );
                assert!(
                    surface.calls.is_empty(),
                    "nothing may be said about a registration that is not happening: {:?}",
                    surface.calls
                );
            }
        }

        // Non-vacuity: the gate is a shape check, not a blanket refusal.
        for accepted in [
            "https://api.moonshot.ai/v1",
            "http://localhost:11434/v1",
            "http://[::1]:11434/v1",
            "HTTPS://API.MOONSHOT.AI/v1",
            "https://user:pw@gw.example.com/v1",
        ] {
            let mut surface = RecordingSurface::new();
            settle_endpoint(
                "kimi",
                ProviderKind::OpenaiCompatible,
                Some(accepted.to_owned()),
                &mut surface,
            )
            .unwrap_or_else(|e| panic!("`{accepted}` is a well-formed endpoint: {e}"));
        }

        // `--kind local` is exempt: its endpoint is ignored by the daemon, and
        // refusing one would break a shape that has always been accepted.
        let mut surface = RecordingSurface::new();
        assert!(settle_endpoint(
            "on-device",
            ProviderKind::Local,
            Some("not a url".to_owned()),
            &mut surface
        )
        .is_ok());
    }

    /// The rendered host is the dialled host, for the one shape where a naive
    /// reading disagrees (REQ-578 verify).
    ///
    /// `https://evil.example\@127.0.0.1/x` is a request to `evil.example`:
    /// WHATWG ends the authority at the backslash. A renderer that stopped at
    /// `/?#` would take the userinfo off at the last `@` and print `127.0.0.1` —
    /// naming a host the request will not reach, which is worse than printing
    /// nothing. The registration seam refuses this shape outright now, but doctor
    /// renders whatever a hand-edited config holds, so the display has to be
    /// right on its own.
    #[test]
    fn a_backslash_authority_renders_the_host_the_request_would_reach() {
        let shown = displayed_endpoint("https://evil.example\\@127.0.0.1/x");
        assert!(
            shown.contains("evil.example"),
            "the dialled host must be the rendered host: {shown}"
        );
        assert!(
            !shown.starts_with("https://127.0.0.1"),
            "the display must not present the post-`@` text as the host: {shown}"
        );
        // And the ordinary userinfo case still redacts.
        assert_eq!(
            displayed_endpoint("https://alice:pw@gw.example.com/v1"),
            "https://***@gw.example.com/v1"
        );
    }

    /// A control byte in a hand-edited endpoint is spelled out rather than
    /// executed (REQ-578 verify).
    ///
    /// `provider add` refuses these, so the only way one reaches a surface is a
    /// config somebody wrote by hand — which is precisely doctor's input. A raw
    /// `\r` printed into a terminal overwrites the line that was about to name
    /// the problem.
    #[test]
    fn a_control_byte_in_a_stored_endpoint_is_escaped_before_it_is_printed() {
        let escaped = escaped_endpoint("https://gw.example.com/v1\r\n\tX");
        assert!(
            !escaped.contains('\r') && !escaped.contains('\n') && !escaped.contains('\t'),
            "no raw control byte may reach the terminal: {escaped:?}"
        );
        assert!(escaped.contains("\\r") && escaped.contains("\\n") && escaped.contains("\\t"));
    }

    /// A credential embedded in the endpoint is stored as typed and **printed
    /// redacted** (REQ-578 verify).
    ///
    /// Both halves matter. Dropping the userinfo from what gets stored would
    /// dial an address the user did not ask for; printing it would put a
    /// password into the scrollback, the session recording, and whatever the
    /// user pastes into a bug report.
    #[test]
    fn userinfo_in_an_endpoint_is_stored_but_never_printed() {
        let (stored, surface) = settled(
            ProviderKind::OpenaiCompatible,
            Some("https://alice:hunter2@gw.example.com/v1"),
        );
        assert_eq!(
            stored.as_deref(),
            Some("https://alice:hunter2@gw.example.com/v1/chat/completions"),
            "the stored endpoint is the address the user gave, credential and all"
        );
        let said = surface.lines_of(LineKind::Info).join("\n");
        assert!(
            !said.contains("hunter2") && !said.contains("alice"),
            "no part of the userinfo may reach the surface: {said}"
        );
        assert!(
            said.contains("***@gw.example.com/v1/chat/completions"),
            "and the redaction has to be visible, or the line claims to show the exact URL \
             while showing a different one: {said}"
        );

        // Doctor's advisory renders the same value and owes the same redaction.
        let advisory = base_url_advisory(&teton_protocol::methods::ProviderConfig {
            id: ProviderId::from("gw"),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://alice:hunter2@gw.example.com/v1".to_owned()),
            model: Some("a-model".to_owned()),
            auth_ref: None,
        })
        .expect("a bare `/v1` base URL is advised on");
        assert!(
            !advisory.contains("hunter2"),
            "doctor prints into the same places: {advisory}"
        );
    }

    /// REQ-578 BR-6 / AC-5: doctor names the full request URL for a stored
    /// endpoint that is really a base URL, and says nothing about the rest.
    ///
    /// Both halves are the claim. The flagged one is what a config hand-edited
    /// from a vendor's quickstart looks like, and before this pass its only
    /// symptom was a 404 with nothing in it. The **silent** one is what keeps
    /// the advice worth reading: a gateway at `/llm/proxy` and an endpoint that
    /// already carries the request path are both correct, and an advisory that
    /// fired on them would be noise a user learns to skip past — which is the
    /// state in which the one line that mattered goes unread.
    ///
    /// The end-to-end form (real binary, real daemon, exit status unchanged) is
    /// `cli_e2e`'s `doctor_flags_a_hand_edited_base_url_endpoint_and_stays_green`.
    #[test]
    fn doctor_advises_on_base_url_endpoints_and_is_silent_on_the_rest() {
        use teton_protocol::methods::ProviderConfig;

        let provider = |id: &str, kind: ProviderKind, endpoint: Option<&str>| ProviderConfig {
            id: ProviderId::from(id),
            kind,
            endpoint: endpoint.map(str::to_owned),
            model: Some("a-model".to_owned()),
            auth_ref: None,
        };

        // Counts are assertable here because this table is the test's own. The
        // e2e form deliberately uses `any`/`!any` instead: its `TestDaemon`
        // fixture config already carries a class-(b) `deepseek` endpoint, so a
        // count there would be an assertion about the fixture rather than about
        // the advisory.
        let mut surface = RecordingSurface::new();
        advise_on_base_url_endpoints(
            &[
                // Flagged: the bare-`/v1` paste this REQ exists for.
                provider(
                    "kimi",
                    ProviderKind::OpenaiCompatible,
                    Some("https://api.moonshot.ai/v1"),
                ),
                // Flagged, and the *ambiguous* half of the advisory: a bare
                // origin on an OpenAI-compatible provider. The composed form is
                // right for this vendor and wrong for OpenAI, and the rule
                // cannot tell the two apart — so the line must hedge rather than
                // state a fact about a host it does not know.
                provider(
                    "deepseek",
                    ProviderKind::OpenaiCompatible,
                    Some("https://api.deepseek.com"),
                ),
                // Flagged: the Anthropic kind's own bare shape, whose full form
                // is a *different* path — the advice is per kind, not a suffix.
                provider(
                    "claude",
                    ProviderKind::Anthropic,
                    Some("https://api.anthropic.com"),
                ),
                // Silent: an explicit gateway path (BR-2 class (c)).
                provider(
                    "gw",
                    ProviderKind::OpenaiCompatible,
                    Some("https://gw.example.com/llm/proxy"),
                ),
                // Silent: already the request URL (BR-2 class (a)).
                provider(
                    "openai",
                    ProviderKind::OpenaiCompatible,
                    Some("https://api.openai.com/v1/chat/completions"),
                ),
                // Silent: the on-device tier has no endpoint to advise on.
                provider("on-device", ProviderKind::Local, None),
            ],
            &mut surface,
        );

        let advised = surface.lines_of(LineKind::Notice);
        assert_eq!(
            advised.len(),
            3,
            "exactly the three base-URL shapes are advised on: {advised:?}"
        );
        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "https://api.moonshot.ai/v1/chat/completions"
            ),
            "the advice has to carry the exact full form a user can paste: {advised:?}"
        );
        assert!(
            surface.any_line_contains(LineKind::Notice, "https://api.anthropic.com/v1/messages"),
            "and the Anthropic kind's own path, not its neighbour's: {advised:?}"
        );
        let rendered = advised.join("\n");
        assert!(
            rendered.contains("kimi") && rendered.contains("claude"),
            "each line must name the provider it is about: {rendered}"
        );

        // The wording, pinned per shape, because the difference between the two
        // is the difference between a true sentence and a false one.
        let line = |id: &str| {
            advised
                .iter()
                .find(|line| line.contains(&format!("`{id}`")))
                .unwrap_or_else(|| panic!("no advisory for `{id}`: {advised:?}"))
                .to_string()
        };
        for (id, unambiguous) in [("kimi", true), ("claude", true), ("deepseek", false)] {
            let line = line(id);
            assert_eq!(
                !line.contains("many vendors serve this under `/v1`"),
                unambiguous,
                "`{id}`'s advisory hedges when it must not, or states a fact it cannot know: \
                 {line}"
            );
            // Every line, both shapes: what Teton would do, and what to do about
            // it. These are the two clauses that carry the whole advisory's
            // value, and either could be dropped by an edit to one branch alone.
            assert!(
                line.contains("Teton would store"),
                "`{id}`: the advisory must say what Teton would store, not merely that something \
                 is off: {line}"
            );
            assert!(
                line.contains("re-add the provider") && line.contains("config.toml"),
                "`{id}`: and how to act on it — an advisory with no next step leaves the user \
                 where they were: {line}"
            );
        }
        for (composed, id) in [
            ("https://api.moonshot.ai/v1/chat/completions", "kimi"),
            ("https://api.anthropic.com/v1/messages", "claude"),
            ("https://api.deepseek.com/chat/completions", "deepseek"),
        ] {
            assert!(
                line(id).contains(composed),
                "`{id}`: the composed form has to appear even in the hedged branch — the hedge is \
                 an addition to the answer, not a replacement for it: {}",
                line(id)
            );
        }
        assert!(
            line("deepseek").contains("https://api.deepseek.com/v1/chat/completions")
                && line("deepseek").contains("check your vendor's docs"),
            "the hedged form has to name the `/v1` alternative outright and say where the answer \
             lives — a user told only that vendors differ is left exactly where they were: {}",
            line("deepseek")
        );
        assert!(
            !line("claude").contains("/v1/v1/"),
            "the Anthropic canonical path is versioned already; offering a `/v1` alternative \
             would double it: {}",
            line("claude")
        );
        assert!(
            !rendered.contains(" a openai-compatible") && !rendered.contains(" a anthropic"),
            "the advisory used to interpolate the kind label behind `a`, which reads wrong for \
             every kind it has: {rendered}"
        );
        // Needles that cannot appear in a flagged line's own text: `openai`
        // alone would match the *kind* name every openai-compatible advisory
        // carries, and a silence assertion that can never fail is not one.
        for silent in ["gw.example.com", "llm/proxy", "api.openai.com", "on-device"] {
            assert!(
                !rendered.contains(silent),
                "`{silent}` is a correct endpoint and must not be advised on: {rendered}"
            );
        }
        assert_eq!(
            advised.len(),
            surface.calls.len(),
            "the pass is advisory: it writes notices and nothing else: {:?}",
            surface.calls
        );
    }

    // -----------------------------------------------------------------------
    // BUG-171: a rejected registration must settle the keychain entry it
    // stored. These drive `report_registration_outcome` the way
    // `run_provider_add` does — prior read, then store, then the daemon's
    // answer — with the mock keychain proving what each arm did about it.
    // -----------------------------------------------------------------------

    /// The prior read and the store, sequenced the way `run_provider_add`
    /// sequences them; returns the ref the config carries and the prior state
    /// the undo will consult.
    fn stored_registration(kc: &MockKeychain, id: &str, secret: &str) -> (String, PriorKey) {
        let prior = PriorKey::read(kc, id);
        let config = build_provider_registration(
            id,
            ProviderKind::Anthropic,
            // The value the flow's composition step would have handed it
            // (REQ-578 BR-3), read from the constant rather than re-typed: a
            // fixture that spells a product fact by hand is a second copy of it.
            Some(ANTHROPIC_DEFAULT_ENDPOINT.to_owned()),
            Some("claude-opus-5".to_owned()),
            kc,
            Some(secret),
        )
        .unwrap();
        (config.auth_ref.unwrap(), prior)
    }

    /// The BUG-170 rejection: a remote kind whose endpoint the daemon's
    /// validator refused. Any non-`METHOD_NOT_FOUND` error takes this arm.
    fn rejection() -> Result<ConfigSetResult, RpcError> {
        Err(RpcError::new(
            error_code::INVALID_PARAMS,
            "provider `opus` is a remote provider and must set an `endpoint`",
        ))
    }

    /// AC: the exact BUG-170 sequence — prompt, store, register, refuse — now
    /// takes the stored key back out and says so.
    #[test]
    fn a_rejected_registration_takes_back_the_key_it_stored() {
        let kc = MockKeychain::new();
        let (auth, prior) = stored_registration(&kc, "opus", "sk-typed-this-run");
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            rejection(),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );

        assert!(
            kc.is_empty(),
            "the credential stored for the refused attempt must be gone"
        );
        assert_eq!(kc.deletes(), vec!["opus".to_owned()]);
        assert!(surface.any_line_contains(LineKind::Error, "registration rejected"));
        assert!(
            surface.any_line_contains(LineKind::Notice, "removed from your keychain"),
            "the cleanup must be said out loud: {:?}",
            surface.lines_of(LineKind::Notice)
        );
    }

    /// The reason the undo is restore-or-delete rather than a blind delete: the
    /// keychain account is the provider id, so an id colliding with a live
    /// entry (here, the `/web setup` key) would have that credential destroyed
    /// — the displaced bytes must come back instead.
    #[test]
    fn a_rejected_registration_restores_the_credential_it_displaced() {
        let kc = MockKeychain::new();
        kc.store("web-search", "sk-live-search-key").unwrap();
        let (auth, prior) = stored_registration(&kc, "web-search", "sk-typed-this-run");
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            rejection(),
            "web-search",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );

        assert_eq!(
            kc.stored_secret("web-search").as_deref(),
            Some("sk-live-search-key"),
            "the displaced credential must be back, byte for byte"
        );
        assert!(
            kc.deletes().is_empty(),
            "a restore is a store, not a delete"
        );
        assert!(surface.any_line_contains(LineKind::Notice, "put back"));
    }

    /// A cleanup the keychain refuses is reported with the command that
    /// finishes it — the user is the only one who can act on the store by hand.
    #[test]
    fn a_cleanup_the_keychain_refuses_names_the_manual_command() {
        let kc = MockKeychain::new();
        let (auth, prior) = stored_registration(&kc, "opus", "sk-typed-this-run");
        kc.fail_delete_with("the keychain is locked");
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            rejection(),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );

        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run"),
            "a refused delete leaves the entry where it was"
        );
        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "security delete-generic-password -s teton -a opus"
            ),
            "the manual command must name the entry: {:?}",
            surface.lines_of(LineKind::Notice)
        );
    }

    /// A keychain that would not answer the pre-store read licenses neither
    /// undo — the entry is left alone and the reason is said.
    #[test]
    fn an_unreadable_keychain_leaves_the_typed_key_and_says_why() {
        let kc = MockKeychain::new();
        kc.fail_read_with("the keychain is locked");
        let (auth, prior) = stored_registration(&kc, "opus", "sk-typed-this-run");
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            rejection(),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );

        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run"),
            "neither undo may run when the prior state is unknown"
        );
        assert!(kc.deletes().is_empty());
        assert!(surface.any_line_contains(LineKind::Notice, "could not be read"));
    }

    /// A local provider stored nothing, so a rejection has nothing to clean up
    /// — and no cleanup sentence to render.
    #[test]
    fn a_local_provider_rejection_has_nothing_to_clean_up() {
        let kc = MockKeychain::new();
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            rejection(),
            "local2",
            ProviderKind::Local,
            "—",
            None,
            &kc,
            &mut surface,
        );

        assert!(kc.deletes().is_empty());
        assert!(surface.any_line_contains(LineKind::Error, "registration rejected"));
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "no key was stored, so no keychain sentence is owed"
        );
    }

    /// The success arm keeps the key, names the ref — and claims the keychain
    /// sentence only when a key was actually stored.
    #[test]
    fn a_successful_registration_keeps_the_key_and_names_the_ref() {
        let kc = MockKeychain::new();
        let (auth, prior) = stored_registration(&kc, "opus", "sk-typed-this-run");
        let mut surface = RecordingSurface::new();

        report_registration_outcome(
            Ok(ConfigSetResult { applied: true }),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );
        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run")
        );
        assert!(kc.deletes().is_empty());
        assert!(surface.any_line_contains(LineKind::Info, "keychain://teton/opus"));

        // A registered local provider stored no key, and its line claims none —
        // the old unconditional sentence reported a key under ref `—`.
        let mut local_surface = RecordingSurface::new();
        report_registration_outcome(
            Ok(ConfigSetResult { applied: true }),
            "local2",
            ProviderKind::Local,
            "—",
            None,
            &kc,
            &mut local_surface,
        );
        let lines = local_surface.lines_of(LineKind::Info);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("registered") && !lines[0].contains("keychain"),
            "{}",
            lines[0]
        );
    }

    /// The pre-BUG-171 arms that deliberately keep the key still do — an old
    /// daemon's pending registration will reference the entry once upgraded —
    /// and the not-applied arm now says where the key is instead of leaving a
    /// rotation invisible.
    #[test]
    fn the_keeping_arms_keep_the_key_and_account_for_it() {
        let kc = MockKeychain::new();
        let (auth, prior) = stored_registration(&kc, "opus", "sk-typed-this-run");

        let mut surface = RecordingSurface::new();
        report_registration_outcome(
            Err(RpcError::new(
                error_code::METHOD_NOT_FOUND,
                "no such method",
            )),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );
        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run"),
            "a pending registration keeps its key"
        );
        assert!(kc.deletes().is_empty());
        assert!(surface.any_line_contains(LineKind::Notice, "pending TASK-013"));

        let mut surface = RecordingSurface::new();
        report_registration_outcome(
            Ok(ConfigSetResult { applied: false }),
            "opus",
            ProviderKind::Anthropic,
            &auth,
            Some(&prior),
            &kc,
            &mut surface,
        );
        assert_eq!(
            kc.stored_secret("opus").as_deref(),
            Some("sk-typed-this-run"),
            "an unapplied registration may already reference the entry — no delete is licensed"
        );
        assert!(kc.deletes().is_empty());
        assert!(surface.any_line_contains(LineKind::Notice, &format!("ref {auth}")));
    }

    /// The unanswered-call sentence: the one path where neither undo is safe,
    /// so it must name both the ref and the command that resolves the ambiguity.
    #[test]
    fn the_unanswered_line_names_the_ref_and_the_escape_hatch() {
        let line = registration_unanswered_line("opus", "keychain://teton/opus");
        assert!(line.contains("may or may not be registered"));
        assert!(line.contains("keychain://teton/opus"));
        assert!(line.contains("teton provider list"));
        assert!(line.contains("security delete-generic-password -s teton -a opus"));
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

    /// REQ-572: the capability `read_config_view` folds off the config snapshot
    /// changes what the status field *says* and not whether the row is *drawn*.
    ///
    /// Asserted at the paint, because this is where the two could come apart: a
    /// machine with no `[web]` table — the default everywhere — must go on
    /// drawing exactly what it drew before (REQ-563 BR-1), and so must a
    /// configured machine whose session has not touched the web, which is the
    /// state REQ-563's pty test observes before it engages the row.
    #[test]
    fn the_reported_capability_changes_the_field_and_not_the_row() {
        use teton_protocol::events::{WebCapabilityState, WebTier};

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

        for (capability, expected) in [
            (WebCapabilityState::OffAvailable, "web: off (available)"),
            (
                WebCapabilityState::Ready {
                    tier: WebTier::FetchAnyUrl,
                },
                "web: fetch (configured)",
            ),
        ] {
            ctx.state.web.capability = Some(capability);
            assert_eq!(
                paint_status(&mut ctx, 0),
                0,
                "the machine's configuration is not something this session did"
            );
            // The field exists and is the richer one — what is suppressed is the
            // row, not the vocabulary.
            assert_eq!(ctx.state.web.status_field(), expected);
        }
        assert!(
            surface.calls.is_empty(),
            "nothing at all should have been drawn: {:?}",
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
