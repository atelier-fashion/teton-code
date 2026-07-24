//! The daemon runtime: the assembled engine/router/egress/cost/MCP state the
//! JSON-RPC handlers drive.
//!
//! [`crate::server`] owns the socket spine and the session registry; this module
//! owns everything a prompt turn actually needs — the local [`Engine`] tier, the
//! phase-policy [`Router`], the single egress choke point, the cost ledger, the
//! permission registry, and any registered MCP servers. It is built once at
//! startup from configuration and the environment ([`DaemonRuntime::from_env`])
//! and shared behind an [`Arc`] by every client task, so sessions and cost
//! history outlive any one client (BR-4).
//!
//! ## Injectable seams (why the environment matters)
//!
//! The daemon ships no model weights and holds no API keys, so the runtime is
//! driven entirely by configuration and a small set of environment seams that
//! the acceptance suite (`tests/e2e`) uses to stand the daemon up without a live
//! model or a live provider:
//!
//! - `TETON_LOCAL_SCRIPT` — a file of canned local-model replies (one per turn,
//!   separated by a `---` line). When set, the local tier is a
//!   [`ScriptedFileEngine`] rather than a real llama.cpp engine, so the offline
//!   read→edit→verify path (AC-1) runs deterministically in CI.
//! - `TETON_CONFIG` — the TOML config file (providers, routing, boundaries, and
//!   the `[[mcp_server]]` MCP registrations, ADR-003 / AC-9).
//! - `TETON_MCP_CONFIG` — a JSON file of MCP server configs. This is a
//!   **test/override** seam only: the main TOML is the source of truth for MCP
//!   servers, but when this env var is set it *replaces* the TOML-declared
//!   servers (used by the acceptance harness for isolation). Precedence:
//!   `TETON_MCP_CONFIG` (when set) > `TETON_CONFIG`'s `[[mcp_server]]` table.
//! - `TETON_REPO_ROOT` — the repo the tools are jailed to.
//!
//! ### Gated test seams (DECISION 3)
//!
//! The rest are **test seams, not operator features**. Each is honoured only when
//! [`test_seams_enabled`] is true — a *debug build* with the master switch
//! `TETON_TEST_SEAMS=1` set. A release build refuses them regardless of the
//! environment (and refuses *loudly*, rather than pretending it never saw the
//! request), so a shipped daemon cannot have its catalog swapped, its disk check
//! disabled, its retry ladder shortened, or its hardware fabricated by an
//! environment variable. They exist so the acceptance suite (`tests/e2e`) can
//! stand the daemon up against mocks; nothing in production sets the master
//! switch.
//!
//! - `TETON_CATALOG` — a model-catalog TOML replacing [`Catalog::bundled`]. The
//!   acceptance suite needs a catalog whose artifact is small enough to actually
//!   download in CI *and* whose `sha256`/`size_bytes` are the genuine digest and
//!   length of the bytes a mock host serves — otherwise the verify path
//!   (BR-6/AC-7) could only ever be asserted, never exercised. An unreadable,
//!   unparseable or invalid file falls back to the bundled catalog with a
//!   warning; a valid override prints a prominent warning and drives the
//!   proposal's `fetch_notice` (H-2), so the consent screen says the entries are
//!   not the shipped catalog.
//! - `TETON_DISK_FREE_BYTES` — a *ceiling* on the free space the installer's
//!   preflight sees (BR-7 / AC-6). It may only ever **lower** the real
//!   measurement, never raise it (M-8): a seam that could raise it would be a way
//!   to make a full disk look empty and so disable the check. Distinct from
//!   `TETON_PROBE_DISK_BYTES`, the figure the probe *reports* to the user.
//! - `TETON_DOWNLOAD_RETRY_BASE_MS` — base delay of the download retry ladder
//!   (BR-16). Only the delays shrink: the number of attempts, the doubling and
//!   the jitter are the production ones, so a test exercises the real ladder
//!   without spending its seconds.
//! - `TETON_PROBE_RAM_BYTES` / `TETON_PROBE_DISK_BYTES` / `TETON_PROBE_GPU` /
//!   `TETON_PROBE_FORCE_SLOW_BENCH` — a simulated machine (REQ-544 BR-9 / AC-8),
//!   so the decision table can be driven from a test instead of from whatever
//!   hardware CI happens to provide. Gated for the same reason as the rest and
//!   then some (E-6): `ram_bytes` feeds
//!   [`validate_choice`](crate::model_consent::validate_choice), so a large
//!   enough `TETON_PROBE_RAM_BYTES` would make every catalog entry look like it
//!   fits and suppress BR-3's above-the-floor confirmation — while the "detected
//!   hardware" the consent screen shows would be the environment's fiction rather
//!   than the machine. `TETON_PROBE_FORCE_SLOW_BENCH` likewise publishes
//!   `benchmark` and `stepped_down` stages for measurements that never happened.
//!
//! `TETON_LOCAL_SCRIPT` stays ungated: it supplies an engine rather than
//! *describing* the machine, changes no safety decision, and is how the offline
//! session path is exercised at all.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::{Config, LocalModelConfig};
use teton_core::entities::{
    BoundaryMode, ModelProvider, PrivacyBoundary, ProviderCapabilities, ProviderKind, RoutingPolicy,
};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;

use teton_inference::catalog::Catalog;
use teton_inference::probe::{decide, GpuClass, HardwareProfile, TierDecision, GIB};
use teton_inference::{Completion, Engine, EngineError, GenParams};

use teton_protocol::events::{ModelLifecycle, ModelLifecycleStage, PrivacyAction};
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    ConfigSnapshot, ConfigUpdate, CostGroupView, CostQueryResult, CostReportView,
    ModelConfirmOutcome, ModelConfirmParams, ModelConfirmResult, ModelListResult, ModelSetResult,
    ModelStatusResult, PrivacyBoundaryConfig, PromptTurnResult, ProviderConfig, RoutingRule,
};
use teton_protocol::{
    Phase as ProtoPhase, PrivacyMode, ProviderId, ProviderKind as ProtoProviderKind, SessionId,
    SessionMode,
};

use teton_providers::{
    classify, AnthropicAdapter, CapabilityProfile, FailureAction, FailureClass,
    OpenAiCompatAdapter, OpenAiCompatConfig, Provider,
};

use crate::broadcast::EventBus;
use crate::cost::{CostLedger, CostReport, GroupTotals, PriceTable};
use crate::download::{HttpRangeFetcher, RetryPolicy};
use crate::egress::{inspect, origin_of, Egress, HttpTransport};
use crate::harness::completion::{context_provenance, RemoteProviderSource};
use crate::harness::context::NoopProvenanceHook;
use crate::harness::turn_loop::{run_session_turn_with_source, HarnessError};
use crate::harness::{
    build_system_prompt, ContextManager, LocalEngineSource, PendingPermissions, PermissionConfig,
    PermissionGate, SessionEvents, ToolContext, ToolRegistry,
};
use crate::install::{CapFreeSpace, FetchCause, HostFreeSpace, LifecycleProgress, WeightsInstall};
use crate::keychain::SecretResolver;
use crate::mcp::{McpRegistry, McpServerConfig};
use crate::model_consent::{
    list_entries, no_local_engine_reason, probe_view, selection_view, ConsentOutcome,
    ModelConsentGate, NoInstaller, PendingModelDecisions, WeightsInstaller,
};
use crate::router::Router;
use crate::selection_store::SelectionStore;

/// Separator between reply blocks in a `TETON_LOCAL_SCRIPT` file.
const SCRIPT_SEPARATOR: &str = "---";

/// A placeholder a scripted reply may contain to force its continuation to depend
/// on the **real** tool output of the current turn's context.
///
/// When [`ScriptedFileEngine::complete`] sees this token in a reply block it
/// substitutes the body of the most recent tool-result block found in the
/// assembled prompt. If no tool result is present — e.g. because a
/// tool-result-plumbing regression discarded it before it reached context — the
/// token resolves to the empty string, so a reply written as `"…: {{LAST_TOOL_RESULT}}"`
/// stops echoing that output and any assertion on it fails. This is what lets the
/// AC-9 e2e prove the MCP tool's result actually reaches the model context, not
/// merely that the tool was offered and gated.
const LAST_TOOL_RESULT_PLACEHOLDER: &str = "{{LAST_TOOL_RESULT}}";

/// The body of the most recent tool-result block in an assembled flat prompt.
///
/// The flat rendering ([`crate::harness::context::ContextManager::assemble`])
/// separates blocks with a blank line and renders a tool result as
/// `Tool (<name>):\n<body>`. This scans the blocks in reverse for the last such
/// header and returns its body, or `""` when the context holds no tool result.
fn last_tool_result_body(prompt: &str) -> &str {
    prompt
        .rsplit("\n\n")
        .find_map(|block| {
            let rest = block.strip_prefix("Tool (")?;
            let (_tool, body) = rest.split_once(":\n")?;
            Some(body)
        })
        .unwrap_or("")
}

/// A local [`Engine`] that replays a fixed script of replies, one per turn.
///
/// This is the CI/offline stand-in for a real llama.cpp engine: the daemon ships
/// no weights, so the acceptance suite points `TETON_LOCAL_SCRIPT` at a file of
/// canned replies (tool calls and a final answer) and the offline read→edit→verify
/// path runs deterministically. When the script is exhausted it returns a
/// plain-text end-of-turn so no runaway loop can outrun it.
pub struct ScriptedFileEngine {
    model_id: String,
    replies: Vec<String>,
    calls: AtomicUsize,
}

impl ScriptedFileEngine {
    /// Parse a script file into per-turn reply blocks (separated by a `---` line).
    ///
    /// # Errors
    /// Returns an I/O error if the file cannot be read.
    pub fn from_file(model_id: impl Into<String>, path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(Self::from_script(model_id, &raw))
    }

    /// Parse a script string into per-turn reply blocks.
    #[must_use]
    pub fn from_script(model_id: impl Into<String>, script: &str) -> Self {
        let replies = script
            .split(&format!("\n{SCRIPT_SEPARATOR}\n"))
            .map(|block| block.trim().to_owned())
            .filter(|block| !block.is_empty())
            .collect();
        Self {
            model_id: model_id.into(),
            replies,
            calls: AtomicUsize::new(0),
        }
    }
}

impl Engine for ScriptedFileEngine {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, EngineError> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .replies
            .get(idx)
            .cloned()
            .unwrap_or_else(|| "Done.".to_owned());
        // A reply may quote the current turn's real tool output via the
        // placeholder, so the scripted continuation genuinely depends on the
        // result reaching context (AC-9 execution proof).
        let text = if text.contains(LAST_TOOL_RESULT_PLACEHOLDER) {
            text.replace(LAST_TOOL_RESULT_PLACEHOLDER, last_tool_result_body(prompt))
        } else {
            text
        };

        let mut completion_tokens = 0u32;
        for token in text.split_inclusive(' ') {
            if completion_tokens >= params.max_tokens {
                break;
            }
            on_token(token);
            completion_tokens += 1;
        }
        let prompt_tokens = u32::try_from(prompt.split_whitespace().count()).unwrap_or(u32::MAX);
        Ok(Completion {
            text,
            prompt_tokens,
            completion_tokens,
        })
    }
}

/// Per-session privacy taint — the BR-1 backstop (REQ-544 C-2).
///
/// Once any tool result's provenance intersects a `local-only` boundary **or** is
/// unknown (a `shell` result), the session is marked tainted and pinned to the
/// local tier for every subsequent turn: the daemon consults this before
/// resolving a route and forces local regardless of phase policy or heuristic.
/// This is what catches the residual the per-request provenance check cannot — a
/// model paraphrasing boundary content it read on an earlier turn — because the
/// whole session is held local once it has seen boundary/unknown content. Shared
/// across turns via the [`DaemonRuntime`] `Arc`, so the pin lives as long as the
/// session (BR-4).
#[derive(Debug, Default)]
pub struct SessionTaint {
    tainted: Mutex<HashSet<SessionId>>,
}

impl SessionTaint {
    /// An empty taint set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `session` tainted — pinned to the local tier for all later turns
    /// (idempotent).
    pub fn mark(&self, session: &SessionId) {
        self.tainted
            .lock()
            .expect("taint mutex poisoned")
            .insert(session.clone());
    }

    /// Whether `session` is pinned to the local tier by a prior boundary/unknown
    /// exposure.
    #[must_use]
    pub fn is_tainted(&self, session: &SessionId) -> bool {
        self.tainted
            .lock()
            .expect("taint mutex poisoned")
            .contains(session)
    }
}

/// Default half-open cooldown for a provider marked [`ProviderHealth::Unavailable`]
/// by a persistent failure (a malformed response, a repeated protocol break)
/// (REQ-544 M-5). Once this window has elapsed the provider is re-probed on the
/// next turn instead of being stranded until daemon restart.
const PROVIDER_UNAVAILABLE_COOLDOWN: Duration = Duration::from_secs(60);

/// Shorter half-open cooldown for a provider taken down by an auth-shaped client
/// error (401/403) (REQ-544 M-5). A credential problem is the kind of fault an
/// operator fixes out of band (rotating a key, fixing an `auth_ref`), so we
/// re-probe sooner rather than stranding it for the full window — the "narrowed
/// persistence" the hardening pass calls for.
const PROVIDER_AUTH_COOLDOWN: Duration = Duration::from_secs(20);

/// A provider's persisted cross-turn health plus, for an `Unavailable` provider,
/// the instant it becomes eligible for a half-open re-probe (REQ-544 M-5).
///
/// This is the fix for the permanent-stranding regression: an `Unavailable`
/// provider is never *selected* by the policy evaluator, so on its own it could
/// never serve a turn, never reset to `Healthy`, and stay down daemon-wide until
/// restart. Recording *when* it went down lets [`Self::effective_health`] present
/// it as eligible again (half-open) once its cooldown elapses; the next turn
/// re-probes it — success records [`Self::healthy`], a fresh failure records a new
/// `Unavailable` with a new deadline.
#[derive(Debug, Clone, Copy)]
struct HealthRecord {
    /// The persisted health state.
    health: ProviderHealth,
    /// For an `Unavailable` record, the instant it may be re-probed. `None` for
    /// `Healthy`/`Degraded` (always eligible).
    retry_at: Option<Instant>,
}

impl HealthRecord {
    /// A healthy record (always eligible).
    fn healthy() -> Self {
        Self {
            health: ProviderHealth::Healthy,
            retry_at: None,
        }
    }

    /// A degraded record — kept in rotation with a reduced profile (always
    /// eligible; the half-open cooldown is only for `Unavailable`).
    fn degraded() -> Self {
        Self {
            health: ProviderHealth::Degraded,
            retry_at: None,
        }
    }

    /// An `Unavailable` record that becomes eligible for a half-open re-probe at
    /// `now + cooldown`.
    fn unavailable(now: Instant, cooldown: Duration) -> Self {
        Self {
            health: ProviderHealth::Unavailable,
            retry_at: Some(now + cooldown),
        }
    }

    /// The health this record presents to routing at `now`, applying the half-open
    /// cooldown: an `Unavailable` provider past its `retry_at` deadline is offered
    /// as `Healthy` so the next turn re-probes it; every other state passes through
    /// unchanged.
    fn effective_health(self, now: Instant) -> ProviderHealth {
        match self.health {
            ProviderHealth::Unavailable => match self.retry_at {
                Some(at) if now >= at => ProviderHealth::Healthy,
                _ => ProviderHealth::Unavailable,
            },
            other => other,
        }
    }
}

/// The assembled daemon runtime shared by every client task.
pub struct DaemonRuntime {
    /// The live configuration (providers, routing, boundaries). Mutated by
    /// `config/set`; read by `config/get` and every routed turn.
    config: Mutex<Config>,
    /// Where the config is persisted on mutation, if a path was resolved.
    config_path: Option<PathBuf>,
    /// The repo the built-in tools are jailed to.
    repo_root: PathBuf,
    /// The local tier, or `None` on a machine below the hardware floor / with no
    /// scripted engine (remote-only operation).
    engine: Option<Arc<Mutex<dyn Engine>>>,
    /// Whether the local tier can meet its BR-8 latency duty right now.
    local_available: bool,
    /// The REQ-547 first-run consent gate: the probe, the catalog, the recorded
    /// decision, the pending-answer registry, and the installer.
    consent: Arc<ModelConsentGate>,
    /// Whether the local tier is **withheld pending a consent decision** (D-3).
    ///
    /// Separate from `local_available`, which answers "can the tier meet its
    /// latency duty"; this answers "has the user agreed to install it at all".
    /// Held as an atomic because the decision arrives asynchronously, long after
    /// the runtime was assembled, and every client task shares one runtime.
    ///
    /// The gate withholds the **tier**, never the session: while it is set,
    /// sessions still run — they route remote-only (BR-1).
    local_gated: AtomicBool,
    /// Whether this daemon's local engine was supplied out of band by
    /// `TETON_LOCAL_SCRIPT` — canned replies from a file, downloading nothing.
    ///
    /// The one sanctioned reason to skip the consent flow, and it is named
    /// rather than inferred (E-5). It used to be spelled `engine.is_none()`,
    /// which happened to be equivalent only because the scripted engine is the
    /// *only* engine this build can construct: the day a real GGUF loader lands
    /// (the tracked REQ-544 debt), that spelling would have disabled the consent
    /// gate and its deep verification on exactly the machines where downloading
    /// weights finally means something. Consent gates *fetching weights*; this
    /// flag says "there are no weights to fetch", which is a different claim
    /// from "there is no engine".
    scripted_engine: bool,
    /// The append-only cost ledger (BR-2). Recorded at the egress choke point.
    ledger: CostLedger,
    /// Daemon-wide registry of in-flight permission prompts (the
    /// `permission/respond` seam).
    pending: Arc<PendingPermissions>,
    /// Per-tool permission policy for every session.
    permission_config: PermissionConfig,
    /// Registered MCP servers (ADR-003), or `None` when none are configured.
    mcp_servers: Vec<McpServerConfig>,
    /// The startup hardware probe's *facts*, or `None` for a runtime with no
    /// local tier at all (the minimal/consent-only runtimes).
    ///
    /// Deliberately the facts and not a rendered event list: the sequence is
    /// replayed to every client that attaches, at whatever time it attaches, so
    /// it is derived fresh from the probe **and the current consent state**
    /// ([`Self::lifecycle_events`]). A stored list would go stale the moment the
    /// user answered — a client attaching after an install would be told the
    /// daemon was still awaiting a decision, which is the same class of untruth
    /// the synthetic `download`/`ready` sequence was.
    probe: Option<ProbeResult>,
    /// Monotonic turn-id source.
    turn_counter: AtomicU64,
    /// Per-session privacy taint: sessions pinned to the local tier because their
    /// context touched `local-only` or unknown-provenance content (REQ-544 C-2).
    session_taint: SessionTaint,
    /// Daemon-wide provider health, persisted across turns (REQ-544 M-5). Updated
    /// by turn outcomes and READ by [`Self::run_prompt_turn`] when it seeds the
    /// router each turn, so a provider observed `Unavailable` stays `Unavailable`
    /// into the next turn's route resolution — activating the policy layer's
    /// cross-turn health fallback. Each entry carries a [`HealthRecord`] so an
    /// `Unavailable` provider becomes eligible for a half-open re-probe once its
    /// cooldown elapses (rather than being stranded until daemon restart). Absent
    /// id ⇒ `Healthy`.
    provider_health: Mutex<BTreeMap<String, HealthRecord>>,
    /// Resolves a provider's `auth_ref` to its secret at call time (BR-7, REQ-544
    /// M-3). Holds the OS-keychain backend behind a trait; the secret is injected
    /// as an endpoint-bound authorization header at the egress choke point and
    /// never reaches a log, `CostRecord`, or telemetry.
    secret_resolver: SecretResolver,
}

impl DaemonRuntime {
    /// A minimal runtime with no local tier, an empty config, and an in-memory
    /// ledger. Used by [`crate::server::Daemon::new`] where no prompt turns run
    /// (the skeleton session-registry tests).
    #[must_use]
    pub fn minimal() -> Self {
        let ledger =
            CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(crate::cost::NoopCostSink))
                .expect("in-memory ledger");
        // A minimal runtime has no local tier at all, so its consent gate records
        // in memory, installs nothing, and probes a machine below the floor —
        // there is nothing for a decision to be about.
        let consent = Arc::new(ModelConsentGate::new(
            HardwareProfile {
                ram_bytes: 0,
                free_disk_bytes: 0,
                gpu: GpuClass::Cpu,
            },
            Catalog::bundled(),
            LocalModelConfig::default(),
            Arc::new(EventBus::new()),
            Arc::new(PendingModelDecisions::new()),
            Arc::new(SelectionStore::in_memory()),
            Arc::new(NoInstaller),
        ));
        Self {
            config: Mutex::new(Config::default()),
            config_path: None,
            repo_root: std::env::temp_dir(),
            engine: None,
            local_available: false,
            consent,
            local_gated: AtomicBool::new(false),
            scripted_engine: false,
            ledger,
            pending: Arc::new(PendingPermissions::new()),
            permission_config: PermissionConfig::coding_defaults(),
            mcp_servers: Vec::new(),
            probe: None,
            turn_counter: AtomicU64::new(0),
            session_taint: SessionTaint::new(),
            provider_health: Mutex::new(BTreeMap::new()),
            secret_resolver: SecretResolver::with_default_backend(),
        }
    }

    /// Build the runtime from configuration and the environment, wiring the cost
    /// ledger's event sink and the egress privacy sink to `events`.
    ///
    /// `base_dir` is the daemon's per-user state directory (where the socket and
    /// the persistent cost ledger live).
    ///
    /// # Errors
    /// Returns an error if the cost ledger cannot be opened.
    pub fn from_env(base_dir: &Path, events: &Arc<EventBus>) -> anyhow::Result<Self> {
        // --- config ---
        let config_path = std::env::var_os("TETON_CONFIG")
            .map(PathBuf::from)
            .or_else(|| Some(base_dir.join("config.toml")));
        // H-1: a present-but-invalid config refuses to start rather than failing
        // open to an empty default that would drop every declared privacy
        // boundary. A genuinely absent file still defaults.
        let config = load_config(config_path.as_deref())?;

        // --- repo root (the tool jail) ---
        let repo_root = std::env::var_os("TETON_REPO_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(std::env::temp_dir);

        // --- cost ledger (BR-2): file-backed so history survives restarts ---
        let cost_sink: Arc<dyn crate::cost::CostEventSink> = events.clone();
        let ledger =
            CostLedger::open(base_dir.join("cost.db"), PriceTable::bundled(), cost_sink)
                .or_else(|_| CostLedger::open_in_memory(PriceTable::bundled(), events.clone()))?;

        // --- local tier: hardware probe (REQ-544 BR-9 / AC-8) + scripted engine ---
        let profile = probe_profile();
        let (catalog, catalog_overridden) = load_catalog();
        // The effective pin is `[local_model] pinned` (REQ-544's top-level key is
        // hard-deprecated and rejected by validation — Decision 2). Resolving it
        // once here means the probe, the consent gate, and `model/list` cannot
        // disagree about which pin is in force.
        let pinned = config.effective_pinned_local_model().map(str::to_owned);
        let probe = probe_local_tier(&profile, &catalog, pinned.as_deref());
        let local = build_local_engine(&probe);
        // E-5: the *kind* of engine, recorded explicitly. Only a scripted engine
        // exempts this daemon from the consent flow, and only because it fetches
        // nothing — not because an engine happens to exist.
        let scripted_engine = local.as_ref().is_some_and(|local| local.scripted);
        let engine: Option<Arc<Mutex<dyn Engine>>> = local.map(|local| local.engine);
        let local_available = engine.is_some() && !probe.disabled;

        // --- first-run consent (REQ-547) ---
        //
        // Assembled but NOT run here: `from_env` must return promptly so the
        // daemon can serve sessions while a proposal is outstanding (D-3). The
        // flow is driven by `run_model_consent`, which `main` spawns.
        let mut local_model = config.local_model.clone();
        local_model.pinned = pinned;
        let consent = Arc::new(
            ModelConsentGate::new(
                profile,
                catalog,
                local_model,
                Arc::clone(events),
                Arc::new(PendingModelDecisions::new()),
                Arc::new(SelectionStore::open(base_dir)),
                build_installer(base_dir, config.local_model.base_url.clone(), events),
            )
            // M-1: gate the gate's `ready` publish on the presence of an engine
            // that can load the weights — the same signal `startup_lifecycle`
            // uses — so a completed install on a no-engine build says `disabled`,
            // not `ready`.
            .with_local_engine(engine.is_some())
            // H-2: a non-bundled catalog is a redirected source; the proposal's
            // `fetch_notice` tells the user so before they answer.
            .with_catalog_override(catalog_overridden),
        );
        let local_gated = AtomicBool::new(local_tier_gated(
            scripted_engine,
            consent.consent_required(),
        ));

        // --- MCP servers (ADR-003 / AC-9): the main TOML config is the source of
        // truth; TETON_MCP_CONFIG is a test-only override (see `load_mcp_servers`).
        let mcp_servers = load_mcp_servers(&config);

        Ok(Self {
            config: Mutex::new(config),
            config_path,
            repo_root,
            engine,
            local_available,
            consent,
            local_gated,
            scripted_engine,
            ledger,
            pending: Arc::new(PendingPermissions::new()),
            permission_config: PermissionConfig::coding_defaults(),
            mcp_servers,
            probe: Some(probe),
            turn_counter: AtomicU64::new(0),
            session_taint: SessionTaint::new(),
            provider_health: Mutex::new(BTreeMap::new()),
            secret_resolver: SecretResolver::with_default_backend(),
        })
    }

    /// A runtime wired for the local-tier consent flow and nothing else.
    ///
    /// The `model/*` handlers read only the consent gate, so this is what the
    /// consent tests stand a [`crate::server::Daemon`] up on — a full
    /// [`Self::from_env`] would drag in the environment, the real state
    /// directory, and the bundled catalog's real digests.
    ///
    /// The tier is marked *capable* (`local_available`) so that the consent gate
    /// is the only thing that can withhold it: a test asserting "undecided ⇒
    /// remote-only" must be observing the gate, not a machine that had no local
    /// tier to begin with.
    #[must_use]
    pub fn with_consent(consent: Arc<ModelConsentGate>) -> Self {
        let gated = local_tier_gated(false, consent.consent_required());
        Self {
            local_available: true,
            local_gated: AtomicBool::new(gated),
            consent,
            ..Self::minimal()
        }
    }

    /// The daemon-wide pending-permission registry (the `permission/respond` seam).
    #[must_use]
    pub fn pending(&self) -> &Arc<PendingPermissions> {
        &self.pending
    }

    /// The first-run consent gate for the local tier (REQ-547).
    #[must_use]
    pub fn consent(&self) -> &Arc<ModelConsentGate> {
        &self.consent
    }

    /// Whether the local tier may serve a turn right now.
    ///
    /// Two independent conditions: the tier must be *capable* (`local_available`,
    /// BR-8's latency duty) and it must be *consented to* (REQ-547 BR-1). A
    /// machine awaiting an answer routes remote-only rather than blocking — the
    /// gate withholds the tier, never the session (D-3).
    #[must_use]
    pub fn local_tier_available(&self) -> bool {
        self.local_available && !self.local_gated.load(Ordering::SeqCst)
    }

    /// Whether the first-run consent flow applies to this daemon at all.
    ///
    /// It does not when the local tier's engine was supplied out of band — a
    /// `TETON_LOCAL_SCRIPT` stand-in replays canned replies from a file and
    /// downloads nothing, so proposing a download would prompt the user for
    /// something that is never going to happen. Consent gates *fetching weights*;
    /// where there are no weights to fetch there is nothing to consent to.
    ///
    /// Keyed on that specific exemption (E-5), never on "this build has no
    /// engine": a daemon that CAN load a GGUF is exactly the daemon that must
    /// ask before downloading one.
    #[must_use]
    pub fn first_run_consent_applies(&self) -> bool {
        !self.scripted_engine
    }

    /// Drive the first-run consent flow to a decision (REQ-547 BR-1).
    ///
    /// Awaits a client's `model/confirm` when a proposal is needed, so callers
    /// must run it off the path that serves requests — `main` spawns it. On a
    /// decided-and-installed outcome the local tier is un-gated for every
    /// subsequent turn.
    pub async fn run_model_consent(self: &Arc<Self>) -> ConsentOutcome {
        let outcome = self.consent.resolve().await;
        self.apply_consent_outcome(&outcome);
        outcome
    }

    /// Install the weights for the decision already recorded (`model/set`).
    pub async fn install_selected_model(self: &Arc<Self>) -> ConsentOutcome {
        let outcome = self.consent.install_recorded().await;
        self.apply_consent_outcome(&outcome);
        outcome
    }

    /// Open or close the tier gate according to a consent outcome.
    ///
    /// Only a `Ready` outcome opens it. A refusal, a failed install, and an
    /// unanswered proposal all leave the tier withheld and the session
    /// remote-only, which is the BR-1 default rather than a special case.
    ///
    /// A `Superseded` outcome (M-4) and an `AlreadyInstalling` outcome (M-2) are
    /// the two cases that must NOT touch the gate: in both, another task is the
    /// authority on the tier — the `model/set` that superseded the first-run
    /// proposal, or the in-flight install this attempt deferred to — so this
    /// abandoned flow leaves the gate exactly as it found it rather than racing
    /// the authoritative decision (an `AlreadyInstalling` no-op that re-gated the
    /// tier would fight the running install that is about to un-gate it).
    fn apply_consent_outcome(&self, outcome: &ConsentOutcome) {
        if matches!(
            outcome,
            ConsentOutcome::Superseded | ConsentOutcome::AlreadyInstalling { .. }
        ) {
            return;
        }
        self.local_gated
            .store(!outcome.local_tier_ready(), Ordering::SeqCst);
    }

    /// The catalog with each entry's fit for this machine (`model/list`, AC-9).
    #[must_use]
    pub fn model_list(&self) -> ModelListResult {
        let consent = &self.consent;
        let decision = consent.probe_decision();
        ModelListResult {
            probe: probe_view(consent.profile(), &decision),
            models: list_entries(consent.profile(), consent.catalog()),
            selection: consent.current_selection().as_ref().map(selection_view),
        }
    }

    /// The recorded decision, the weights' install state, and any outstanding
    /// proposal (`model/status`, AC-9).
    ///
    /// `pending_proposal` carries the proposal **in full** — the same payload the
    /// `model_selection_proposed` event carries. That is what lets a client which
    /// attached *after* the broadcast render the pick by name, with its download
    /// size and RAM floor (BR-2), and answer it — rather than waiting forever for
    /// an event it already missed, or answering a prompt it could only describe
    /// as "the daemon's own pick".
    #[must_use]
    pub fn model_status(&self) -> ModelStatusResult {
        ModelStatusResult {
            selection: self
                .consent
                .current_selection()
                .as_ref()
                .map(selection_view),
            install: self.consent.current_install(),
            pending_proposal: self.consent.pending().outstanding(),
        }
    }

    /// Change the selected model after first run (`model/set`, AC-9 / BR-3).
    ///
    /// # Errors
    /// Returns a [`RpcError`] (`INVALID_PARAMS`) naming an unknown catalog entry,
    /// or an above-RAM-floor pick that has not been confirmed a second time.
    pub fn set_model(
        &self,
        name: &str,
        confirmed_above_ram_floor: bool,
    ) -> Result<ModelSetResult, RpcError> {
        let selection = self
            .consent
            .set_model(name, confirmed_above_ram_floor)
            .map_err(|refusal| RpcError::new(error_code::INVALID_PARAMS, refusal.to_string()))?;
        Ok(ModelSetResult {
            selection: selection_view(&selection),
        })
    }

    /// Deliver a client's answer to an outstanding proposal (`model/confirm`).
    ///
    /// A `choose` is validated **before** the waiter is resolved, so a bad answer
    /// comes back as an RPC error the client can correct while the proposal stays
    /// open — a mistyped model name must not cost the user their prompt (BR-3).
    ///
    /// # Errors
    /// Returns a [`RpcError`] (`INVALID_PARAMS`) for a refused choice.
    pub fn confirm_model(
        &self,
        params: ModelConfirmParams,
    ) -> Result<ModelConfirmResult, RpcError> {
        match &params.outcome {
            ModelConfirmOutcome::Choose {
                name,
                confirmed_above_ram_floor,
            } => {
                crate::model_consent::validate_choice(
                    self.consent.catalog(),
                    self.consent.profile(),
                    name,
                    *confirmed_above_ram_floor,
                )
                .map_err(|refusal| {
                    RpcError::new(error_code::INVALID_PARAMS, refusal.to_string())
                })?;
            }
            // Pre-validate an `accept` the same way a `choose` is pre-validated,
            // and against the same two rules.
            //
            // If the outstanding proposal offered no model (this machine has no
            // fitting catalog entry), there is nothing to accept. And if it
            // proposed an entry above this machine's RAM floor — which a
            // `[local_model] pinned` key can do, since a pin overrides the probe
            // unconditionally and since C-1 reaches the user as the proposal
            // itself — then BR-3's second confirmation is owed before a
            // multi-gigabyte fetch begins, and an `accept` does not carry one
            // (E-1).
            //
            // Both are rejected as INVALID_PARAMS with the proposal LEFT OPEN,
            // rather than letting the accept resolve the waiter and fail inside
            // the flow: that would permanently consume the user's one chance to
            // answer and leave the tier dead for the daemon's lifetime. Left
            // open, the client re-sends the same entry as
            // `choose { confirmed_above_ram_floor: true }`.
            ModelConfirmOutcome::Accept => {
                if let Some(open) = self.consent.pending().outstanding() {
                    let Some(proposed) = open.proposed.as_ref() else {
                        return Err(RpcError::new(
                            error_code::INVALID_PARAMS,
                            crate::model_consent::ChoiceRefusal::NothingToAccept.to_string(),
                        ));
                    };
                    crate::model_consent::validate_choice(
                        self.consent.catalog(),
                        self.consent.profile(),
                        &proposed.entry.name,
                        false,
                    )
                    .map_err(|refusal| {
                        RpcError::new(error_code::INVALID_PARAMS, refusal.to_string())
                    })?;
                }
            }
            ModelConfirmOutcome::Decline => {}
        }
        // Idempotent, like `permission/respond`: a late or duplicate answer for a
        // proposal that already resolved simply finds no waiter. E-8: say which
        // it was, so a client whose prompt was cancelled by a `model/set` is not
        // told its answer landed.
        let delivered = self
            .consent
            .pending()
            .resolve(&params.request_id, params.outcome);
        Ok(ModelConfirmResult { delivered })
    }

    /// The startup model-lifecycle events (REQ-544 BR-9), replayed to attaching
    /// clients.
    ///
    /// Derived per call, from the probe *and* the consent state as it stands
    /// right now — see [`startup_lifecycle`]. A client attaching before the user
    /// answers is told the daemon is awaiting a decision; one attaching after an
    /// install is told what is actually on disk. Both are true when they are
    /// said, which a snapshot taken at startup could not be.
    #[must_use]
    pub fn lifecycle_events(&self) -> Vec<ModelLifecycle> {
        match &self.probe {
            Some(probe) => startup_lifecycle(probe, self.engine.is_some(), &self.consent),
            None => Vec::new(),
        }
    }

    /// A snapshot of the current configuration for `config/get`.
    #[must_use]
    pub fn config_snapshot(&self) -> ConfigSnapshot {
        let config = self.config.lock().expect("config mutex poisoned");
        snapshot_from_config(&config)
    }

    /// Apply a `config/set` mutation, validate, and persist it.
    ///
    /// # Errors
    /// Returns a [`RpcError`] (code `CONFIG_REJECTED`) if the resulting config
    /// fails validation (e.g. a raw key in `auth_ref`, BR-7).
    pub fn apply_config_update(&self, update: ConfigUpdate) -> Result<(), RpcError> {
        let mut config = self.config.lock().expect("config mutex poisoned");
        let mut candidate = config.clone();
        apply_update(&mut candidate, update);
        candidate
            .validate()
            .map_err(|e| RpcError::new(error_code::CONFIG_REJECTED, e.to_string()))?;
        if let Some(path) = &self.config_path {
            if let Ok(toml) = candidate.to_toml() {
                let _ = std::fs::write(path, toml);
            }
        }
        *config = candidate;
        Ok(())
    }

    /// The authoritative cost report for `cost/query` (BR-2 / AC-4).
    ///
    /// # Errors
    /// Returns a [`RpcError`] if the ledger cannot be read.
    pub fn cost_report(&self) -> Result<CostQueryResult, RpcError> {
        let report = self
            .ledger
            .report()
            .map_err(|e| RpcError::new(error_code::INTERNAL_ERROR, e.to_string()))?;
        Ok(CostQueryResult {
            report: cost_report_view(&report),
        })
    }

    /// Record a provider's observed health so it persists into the next turn's
    /// routing (REQ-544 M-5). Downgrades survive across turns: a provider seen
    /// `Unavailable` stays `Unavailable` until either it serves a turn again or its
    /// half-open cooldown elapses (see [`HealthRecord`]).
    fn record_health(&self, provider_id: &str, record: HealthRecord) {
        self.provider_health
            .lock()
            .expect("provider_health mutex poisoned")
            .insert(provider_id.to_owned(), record);
    }

    /// Run one prompt turn for `session`, streaming events over `events` and
    /// returning the turn result.
    ///
    /// This is the daemon-side integration seam: it resolves the route (structured
    /// phase policy or freeform heuristic), builds the appropriate
    /// [`crate::harness::CompletionSource`] (local engine or a remote provider
    /// through the egress choke point), runs the unified turn loop, and — on a
    /// remote failure — falls back per the router (AC-7).
    ///
    /// # Errors
    /// Returns a [`RpcError`] when no provider can serve the turn or an
    /// unrecoverable provider failure occurs.
    pub async fn run_prompt_turn(
        self: &Arc<Self>,
        events: &Arc<EventBus>,
        session_id: SessionId,
        mode: SessionMode,
        phase: Option<ProtoPhase>,
        prompt: String,
    ) -> Result<PromptTurnResult, RpcError> {
        let turn_id = teton_protocol::TurnId::from(format!(
            "turn-{}",
            self.turn_counter.fetch_add(1, Ordering::SeqCst)
        ));

        let config = self.config.lock().expect("config mutex poisoned").clone();
        // REQ-544 M-5: seed the router from the daemon-wide health map so a
        // provider marked Unavailable on an earlier turn stays Unavailable here —
        // UNLESS its half-open cooldown has elapsed, in which case it is offered as
        // Healthy so this turn re-probes it (the recovery path that keeps a single
        // transient failure from stranding a provider daemon-wide until restart).
        let now = Instant::now();
        let health_snapshot: BTreeMap<String, ProviderHealth> = self
            .provider_health
            .lock()
            .expect("provider_health mutex poisoned")
            .iter()
            .map(|(id, record)| (id.clone(), record.effective_health(now)))
            .collect();
        let router = build_router(
            &config,
            // REQ-547 BR-1/D-3: a tier awaiting a consent decision is withheld
            // here, so this turn routes remote-only instead of blocking on the
            // answer.
            self.local_tier_available(),
            self.ledger.prices(),
            &health_snapshot,
        );

        // Resolve the initial route (BR-5): structured -> phase policy; freeform
        // -> heuristic. Emitting `route_decided` is the legibility promise.
        //
        // REQ-544 C-2: a session tainted by earlier boundary/unknown exposure is
        // pinned to the local tier for every subsequent turn — the router forces
        // local regardless of phase policy or heuristic. This is the backstop for
        // the model-paraphrase residual BR-1 provenance alone cannot catch.
        let core_phase = phase.map(to_core_phase);
        let mut route = if self.session_taint.is_tainted(&session_id) {
            router.resolve_local_pin(
                "session previously touched local-only content; pinned to the local tier (BR-1 backstop)",
            )
        } else {
            match mode {
                SessionMode::Structured => {
                    let ph = core_phase.unwrap_or(CorePhase::Implement);
                    router.resolve_structured(ph)
                }
                SessionMode::Freeform => router.resolve_freeform(&prompt),
            }
        };

        // Assemble the harness context, tools, and the permission gate once; a
        // fallback re-runs the loop against the same accumulated context.
        //
        // REQ-544 (known limitation, deliberately deferred): the retry/fallback
        // path below re-runs the loop against this *same* `ctx`, which by design
        // preserves completed work (file reads/edits done before a mid-turn
        // transient failure). The trade-off is that the accumulated context is
        // re-sent to the retry/fallback provider and thus re-billed as input
        // tokens — a mid-turn transient failure re-bills the partial progress.
        // A clean fix (snapshot `ctx` here and restore it before a retry, or drive
        // retries at single-call granularity so only the failed call is re-issued)
        // changes the "continue vs. restart" semantics and needs a product call on
        // whether a fallback should preserve or discard partial work; it is out of
        // scope for this correctness pass. `ContextManager` is `Clone`, so the
        // snapshot itself is cheap when that decision is made.
        // TODO(REQ-544 followup): make retries cost-neutral once continue-vs-restart
        // semantics are decided.
        let tools = self.build_tools(events, &session_id).await;
        let tool_ctx = ToolContext::new(&self.repo_root);
        let gate = PermissionGate::new(
            session_id.clone(),
            self.permission_config.clone(),
            events.clone(),
            self.pending.clone(),
        );
        let stream_events = SessionEvents::new(events.clone(), session_id.clone());

        let system = build_system_prompt(&tools, &route.harness);
        let mut ctx = ContextManager::new(system, route.harness.context_budget_tokens);
        ctx.push_user(prompt);

        let mut attempts = 0u32;
        let mut rerouted_local = false;
        loop {
            router.emit_route_decided(events, Some(session_id.clone()), &route);
            let provider_id = route.provider_id.clone();

            let result = self
                .run_one_attempt(
                    events,
                    &config,
                    &route,
                    &session_id,
                    phase,
                    &tools,
                    &tool_ctx,
                    &gate,
                    &stream_events,
                    &mut ctx,
                )
                .await;

            // REQ-544 M-1: a privacy block is NOT a transient failure. It must
            // never be retried against the blocked provider (which would emit
            // duplicate `privacy_block` events and never reroute). Taint the
            // session and re-run this same turn on the local tier — reusing the
            // C-2 taint→local mechanism — so there is exactly one block event and
            // one reroute. The egress choke point already emitted the single
            // authoritative `privacy_block`.
            if let Err(err) = &result {
                if err.is_privacy_blocked() {
                    self.session_taint.mark(&session_id);
                    if self.engine.is_none() {
                        return Err(RpcError::new(
                            error_code::PRIVACY_BLOCKED,
                            "this turn's content is under a local-only privacy boundary \
                             and no local tier is available to serve it",
                        ));
                    }
                    if rerouted_local {
                        // Already rerouted to local (which has no egress and so
                        // cannot privacy-block) — never loop.
                        return Err(RpcError::new(
                            error_code::PRIVACY_BLOCKED,
                            "privacy boundary blocked this turn and the local reroute \
                             could not serve it",
                        ));
                    }
                    route = router.resolve_local_pin(
                        "remote egress blocked by a local-only boundary; rerouted to the \
                         local tier (BR-1)",
                    );
                    rerouted_local = true;
                    continue;
                }
            }

            match result {
                Ok(outcome) => {
                    // REQ-544 M-5: a provider that just served a turn is healthy
                    // again — clear any earlier downgrade (including a half-open
                    // re-probe that just succeeded) so a recovered provider returns
                    // to full rotation on the next turn.
                    if let Some(pid) = route.provider_id.as_ref() {
                        self.record_health(&pid.0, HealthRecord::healthy());
                    }
                    // REQ-544 C-2: if this turn's context intersects a local-only
                    // boundary or carries unknown provenance, pin the session to
                    // the local tier for every subsequent turn (the backstop for
                    // a later model paraphrase of what it read here).
                    if context_is_sensitive(&ctx, &config.boundaries) {
                        self.session_taint.mark(&session_id);
                    }
                    return Ok(PromptTurnResult {
                        turn_id,
                        stop_reason: outcome.stop_reason,
                    });
                }
                Err(HarnessError::Remote(perr)) if attempts < 2 => {
                    attempts += 1;
                    let Some(pid) = provider_id.as_ref() else {
                        return Err(RpcError::new(
                            error_code::INTERNAL_ERROR,
                            "remote turn failed with no provider to fall back from",
                        ));
                    };
                    let Some(class) = perr.failure_class() else {
                        return Err(RpcError::new(
                            error_code::INTERNAL_ERROR,
                            "provider failed unrecoverably",
                        ));
                    };
                    // REQ-544 M-5: persist the failed provider's health so the
                    // downgrade survives into the next turn's routing. A transient
                    // failure (Retry) leaves health untouched; a persistent one is
                    // stamped with a half-open cooldown so it recovers on its own.
                    if let Some(record) = health_record_after_failure(class, Instant::now()) {
                        self.record_health(&pid.0, record);
                    }
                    let fo = router.on_provider_failure(core_phase, &pid.0, class);
                    if let Some(degraded) = fo.degraded {
                        router.emit_provider_degraded(events, Some(session_id.clone()), degraded);
                    }
                    match fo.route {
                        Some(next) => {
                            route = next;
                            continue;
                        }
                        None => {
                            return Err(RpcError::new(
                                error_code::UNKNOWN_PROVIDER,
                                "provider failed and no fallback is configured",
                            ));
                        }
                    }
                }
                Err(HarnessError::Remote(_)) => {
                    return Err(RpcError::new(
                        error_code::INTERNAL_ERROR,
                        "remote turn failed after exhausting fallbacks",
                    ));
                }
                Err(HarnessError::Engine(_)) => {
                    return Err(RpcError::new(
                        error_code::INTERNAL_ERROR,
                        "local engine could not serve the turn",
                    ));
                }
                // REQ-544 M-3: a credential that will not resolve is a config
                // problem, not a transient fault — surface it clearly (the
                // message names the reference and reason, never the secret) and
                // do not retry the same broken credential.
                Err(HarnessError::Credential(msg)) => {
                    return Err(RpcError::new(error_code::CONFIG_REJECTED, msg));
                }
            }
        }
    }

    /// Build the tool registry for a turn: the built-ins plus any registered MCP
    /// server tools (ADR-003), namespaced and egress-gated.
    async fn build_tools(&self, events: &Arc<EventBus>, session_id: &SessionId) -> ToolRegistry {
        let mut tools = ToolRegistry::with_builtins();
        if !self.mcp_servers.is_empty() {
            let boundaries = self
                .config
                .lock()
                .expect("config mutex poisoned")
                .boundaries
                .clone();
            if let Ok(transport) = HttpTransport::new() {
                let egress = Arc::new(
                    Egress::new(transport, boundaries, events.clone())
                        .with_cost_meter(Arc::new(self.ledger.clone())),
                );
                let registry = Arc::new(McpRegistry::with_egress(
                    egress as Arc<dyn crate::mcp::EgressGate>,
                    Some(session_id.clone()),
                    self.mcp_servers.clone(),
                ));
                crate::harness::tools::mcp::register_mcp_tools(
                    &mut tools,
                    registry,
                    tokio::runtime::Handle::current(),
                )
                .await;
            }
        }
        tools
    }

    /// Run one turn attempt against the route's provider (local or remote).
    #[allow(clippy::too_many_arguments)]
    async fn run_one_attempt(
        &self,
        events: &Arc<EventBus>,
        config: &Config,
        route: &crate::router::Route,
        session_id: &SessionId,
        phase: Option<ProtoPhase>,
        tools: &ToolRegistry,
        tool_ctx: &ToolContext,
        gate: &PermissionGate,
        stream_events: &SessionEvents,
        ctx: &mut ContextManager,
    ) -> Result<crate::harness::TurnOutcome, HarnessError> {
        let mut hook = NoopProvenanceHook;
        let provider_cfg = route
            .provider_id
            .as_ref()
            .and_then(|pid| config.providers.iter().find(|p| p.id == pid.0));

        let is_local = match provider_cfg {
            Some(p) => matches!(p.kind, ProviderKind::Local),
            // No provider selected: fall back to the local tier if present.
            None => self.engine.is_some(),
        };

        if is_local {
            let Some(engine) = self.engine.as_ref() else {
                return Err(HarnessError::Engine(EngineError::unavailable(
                    "no local tier configured",
                )));
            };
            let mut source = LocalEngineSource::new(&**engine);
            return run_session_turn_with_source(
                &mut source,
                tools,
                tool_ctx,
                gate,
                stream_events,
                ctx,
                &route.harness,
                &mut hook,
                Some(&**engine),
            )
            .await;
        }

        // Remote: build the adapter + egress choke point, then drive it.
        let provider_cfg = provider_cfg.ok_or_else(|| {
            HarnessError::Engine(EngineError::unavailable("no provider for this turn"))
        })?;
        let model = route
            .model
            .clone()
            .unwrap_or_else(|| provider_cfg.id.clone());
        let caps = CapabilityProfile::from_core(provider_cfg.capabilities);
        let provider: Box<dyn Provider> = build_provider(provider_cfg, caps);

        // BR-7 / REQ-544 M-3: resolve the provider's credential from its
        // `auth_ref` and bind it to this provider's endpoint. A provider with no
        // `auth_ref` (e.g. a local mock endpoint) gets a credential-free
        // transport, exactly as before. The injected header rides only requests
        // to this endpoint's origin — never MCP, never another provider.
        let transport = build_remote_transport(provider_cfg, &self.secret_resolver)?;
        let boundaries = config.boundaries.clone();
        let egress = Egress::new(transport, boundaries, events.clone())
            .with_cost_meter(Arc::new(self.ledger.clone()));

        let mut source = RemoteProviderSource::new(
            &*provider,
            &egress,
            ProviderId::from(provider_cfg.id.as_str()),
            model,
            session_id.clone(),
        );
        if let Some(ph) = phase {
            source = source.with_phase(ph);
        }

        let summarizer = self.engine.as_deref();
        run_session_turn_with_source(
            &mut source,
            tools,
            tool_ctx,
            gate,
            stream_events,
            ctx,
            &route.harness,
            &mut hook,
            summarizer,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

/// Load the config from `path`.
///
/// A *genuinely absent* config file defaults — a fresh install has none, and
/// defaulting there is correct. But a config that **exists** and fails to parse
/// or validate must NOT be silently replaced by [`Config::default`] (H-1): the
/// default carries `boundaries: vec![]`, so failing open would drop every
/// declared privacy boundary, provider, routing rule and MCP server on the floor
/// and bring the daemon up with a security posture the user never chose — a typo
/// in one field silently disabling every `local-only` boundary. A present-but-
/// invalid config is refused instead, with a diagnostic naming the failure, so
/// the operator fixes it rather than unknowingly running wide open.
///
/// # Errors
/// Returns an error when a config file is present but cannot be read, parsed, or
/// validated. The message names the validation failure but no filesystem path
/// (BR-11).
fn load_config(path: Option<&Path>) -> anyhow::Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(path) {
        // Present and readable: it MUST parse and validate. Refusing here is the
        // whole point — a fail-open default would drop the user's boundaries.
        Ok(text) => Config::load(&text).map_err(|e| {
            anyhow::anyhow!(
                "the daemon configuration is present but invalid, so it was NOT loaded. \
                 Refusing to start rather than fall back to an empty config that would \
                 silently drop your privacy boundaries, providers, routing, and MCP servers. \
                 Fix the config and restart. Cause: {e}"
            )
        }),
        // Genuinely absent (a fresh install): defaulting is correct.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        // Present but unreadable (permissions, I/O): surface it rather than
        // defaulting — the operator has a config they meant to apply.
        Err(err) => Err(anyhow::anyhow!(
            "the daemon configuration file exists but could not be read ({}); \
             refusing to start rather than silently ignore it.",
            err.kind()
        )),
    }
}

/// Resolve the MCP servers this daemon serves (ADR-003 / AC-9).
///
/// The main config document (`[[mcp_server]]`, already validated by
/// [`Config::validate`]) is the **source of truth** — a server registers in one
/// place alongside providers and boundaries. `TETON_MCP_CONFIG`, a JSON array, is
/// a **test/override** seam the acceptance harness uses for isolation: when it is
/// set it *replaces* the TOML-declared servers. Precedence is therefore
/// `TETON_MCP_CONFIG` (when set) > `config.mcp_server`.
fn load_mcp_servers(config: &Config) -> Vec<McpServerConfig> {
    if let Some(path) = std::env::var_os("TETON_MCP_CONFIG") {
        return match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
    }
    config.mcp_server.clone()
}

/// The `model_id` a lifecycle event carries when the machine has no model to
/// name — a below-the-floor probe, or a catalog with nothing that fits.
const LOCAL_TIER_ID: &str = "local";

/// The installer the consent gate hands a decided model to.
///
/// The download client is credential-free and redirect-following (D-2, TASK-002).
/// If it cannot be built at all, the daemon still runs — it just cannot install
/// weights, and says so rather than reporting them as merely absent.
///
/// Three wires matter here and each is load-bearing:
/// - `base_url` is the `[local_model] base_url` override reaching the *fetch*
///   (BR-16). The catalog's `download_url` implements the rewrite, but a
///   configured mirror that never reaches the installer redirects nothing.
/// - the fetcher is handed over twice — once as the transport, once as the
///   [`FetchCause`] the pipeline reads the precise failure back from, so a 429
///   is reported as rate-limiting rather than as a generic transport failure
///   (AC-12).
/// - `events` makes install progress observable as `model_lifecycle` (AC-2).
fn build_installer(
    base_dir: &Path,
    base_url: Option<String>,
    events: &Arc<EventBus>,
) -> Arc<dyn WeightsInstaller> {
    match HttpRangeFetcher::with_policy(download_retry_policy()) {
        Ok(fetcher) => {
            let fetcher = Arc::new(fetcher);
            let cause: Arc<dyn FetchCause> = fetcher.clone();
            let mut install = WeightsInstall::new(
                fetcher,
                base_dir.join(teton_protocol::weights::WEIGHTS_DIR),
                base_url,
            )
            .with_cause(cause)
            .with_progress(Arc::new(LifecycleProgress::new(Arc::clone(events))));
            // AC-6's claim is about behaviour on a full volume, which no CI
            // machine will provide on demand. DECISION 3 + M-8: a test seam,
            // honoured only in a debug build with the master switch, and it may
            // only ever *lower* the measured free space — a seam that could raise
            // it would be a way to disable BR-7, so `CapFreeSpace` takes the
            // minimum of the real measurement and the ceiling.
            if let Some(ceiling) = env_u64("TETON_DISK_FREE_BYTES").filter(|_| test_seams_enabled())
            {
                install = install.with_free_space(Arc::new(CapFreeSpace {
                    inner: Arc::new(HostFreeSpace),
                    ceiling,
                }));
            }
            Arc::new(install)
        }
        Err(_) => Arc::new(NoInstaller),
    }
}

/// The download retry ladder, with only its *delays* overridable (BR-16).
///
/// The attempt count, the doubling and the jitter stay production values: a test
/// that shortened the ladder itself would be exercising a different policy than
/// the one that ships. Shortening the base delay changes how long the same ladder
/// takes, not what it does.
fn download_retry_policy() -> RetryPolicy {
    let default = RetryPolicy::default();
    // DECISION 3: a test seam, honoured only in a debug build with the master
    // switch — never in a shipped daemon.
    match env_u64("TETON_DOWNLOAD_RETRY_BASE_MS").filter(|_| test_seams_enabled()) {
        Some(base_ms) => RetryPolicy {
            base_delay: Duration::from_millis(base_ms),
            max_delay: Duration::from_millis(base_ms.saturating_mul(8)),
            ..default
        },
        None => default,
    }
}

/// What the seam master switch means for this build (DECISION 3 / E-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeamPolicy {
    /// A debug build with the switch on: the seams are honoured.
    Honour,
    /// Nobody asked for them.
    Ignore,
    /// The switch was set in a build that cannot honour it. **Refuse loudly.**
    /// Ignoring it silently is the dangerous answer: whoever set it believes the
    /// daemon is under test control — mocked catalog, simulated hardware, capped
    /// free space — and would read the resulting run as a test result while the
    /// daemon quietly used the real catalog, the real machine, and the real
    /// network. A refusal is a fixable mistake; a silent one is a wrong answer.
    Refuse,
}

/// The policy for a build kind and the raw `TETON_TEST_SEAMS` value.
///
/// Pure so the release-build refusal is testable from a debug-build test — the
/// branch that matters is the one this binary cannot otherwise reach.
fn seam_policy(debug_build: bool, switch: Option<&str>) -> SeamPolicy {
    match (debug_build, switch) {
        (true, Some("1")) => SeamPolicy::Honour,
        // Only the value a debug build would have honoured is a refusal; an
        // explicit `TETON_TEST_SEAMS=0` is someone turning them off, which a
        // release build is entitled to simply agree with.
        (false, Some("1")) => SeamPolicy::Refuse,
        _ => SeamPolicy::Ignore,
    }
}

/// Whether the test seams (`TETON_CATALOG`, `TETON_DISK_FREE_BYTES`,
/// `TETON_DOWNLOAD_RETRY_BASE_MS`, `TETON_PROBE_*`) may be honoured (DECISION 3).
///
/// A **debug build with `TETON_TEST_SEAMS=1`** and nothing else. A release build
/// refuses regardless of the switch — the seams are how the acceptance suite
/// stands the daemon up against mocks, never an operator feature, so a shipped
/// binary must not honour them even if the environment sets them — and it refuses
/// *loudly* (E-6) rather than pretending it never saw the request.
///
/// # Panics
/// Panics when `TETON_TEST_SEAMS=1` is set in a release build.
fn test_seams_enabled() -> bool {
    match seam_policy(
        cfg!(debug_assertions),
        std::env::var("TETON_TEST_SEAMS").ok().as_deref(),
    ) {
        SeamPolicy::Honour => true,
        SeamPolicy::Ignore => false,
        SeamPolicy::Refuse => panic!(
            "tetond: TETON_TEST_SEAMS=1 is set, but this is a release build, which cannot \
             honour the test seams (TETON_CATALOG, TETON_DISK_FREE_BYTES, \
             TETON_DOWNLOAD_RETRY_BASE_MS, TETON_PROBE_*). Refusing to start rather than run \
             as a production daemon while the environment believes it is under test control. \
             Unset TETON_TEST_SEAMS, or use a debug build."
        ),
    }
}

/// The model catalog this daemon proposes from, and whether it is a non-bundled
/// override.
///
/// `TETON_CATALOG` is a **test seam** (DECISION 3): it is honoured only when
/// [`test_seams_enabled`] is true. In a release build, or without the master
/// switch, it is ignored and its use is logged — a shipped daemon always proposes
/// from the catalog it was released with, never one an environment variable
/// swapped in. When an override IS honoured, a prominent warning is printed and
/// the returned flag drives the proposal's `fetch_notice`, so the consent screen
/// says the entries are not the shipped catalog.
///
/// An override that does not parse or does not validate falls back to the bundled
/// catalog with a warning rather than aborting startup: a mistyped path must not
/// brick a daemon, and a *silently* substituted catalog would not be a correct
/// answer, which is why the fallback is announced.
fn load_catalog() -> (Catalog, bool) {
    let Some(path) = std::env::var_os("TETON_CATALOG") else {
        return (Catalog::bundled(), false);
    };
    if !test_seams_enabled() {
        eprintln!(
            "tetond: ignoring TETON_CATALOG — it is a test seam honoured only in a debug build \
             with TETON_TEST_SEAMS=1, not an operator feature. Using the bundled catalog."
        );
        return (Catalog::bundled(), false);
    }
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| Catalog::from_toml(&text).ok())
        .filter(|catalog| catalog.validate().is_ok());
    match parsed {
        Some(catalog) => {
            eprintln!(
                "tetond: WARNING — proposing from an override model catalog (TETON_CATALOG). \
                 This is a test seam, not the shipped catalog; the consent prompt will say so."
            );
            (catalog, true)
        }
        None => {
            eprintln!(
                "tetond: TETON_CATALOG did not name a readable, valid catalog; \
                 using the bundled catalog"
            );
            (Catalog::bundled(), false)
        }
    }
}

/// The result of the startup hardware probe (REQ-544 BR-9 / AC-8).
///
/// Facts only. What the *client* is told about them is
/// [`startup_lifecycle`]'s job, because the honest answer depends on state this
/// function cannot see — whether a decision has been made, whether weights are
/// on disk, and whether anything in this build can load them.
struct ProbeResult {
    /// The local model id in force after any step-down, or `None` when disabled.
    model: Option<String>,
    /// The model the probe itself picked, before a simulated step-down moved off
    /// it. What the `probed` stage names, because that is what was probed.
    probed_model: Option<String>,
    /// Whether the local tier is disabled (below floor / resource-starved).
    disabled: bool,
    /// Why the local tier is disabled, when it is — the probe's own sentence.
    disabled_reason: Option<String>,
    /// Detected system RAM, as quoted in the `probed` stage.
    ram_bytes: u64,
    /// Whether the machine cleared the local-tier RAM floor.
    above_floor: bool,
    /// The `TETON_PROBE_FORCE_SLOW_BENCH` simulation, when it was asked for.
    forced_bench: Option<ForcedBench>,
}

/// A benchmark ladder the operator explicitly asked to have *simulated*
/// (`TETON_PROBE_FORCE_SLOW_BENCH`), so REQ-544's auto-step-down duty is
/// exercisable end to end without a real model.
///
/// It is the one place a `benchmark` stage is published without a measurement,
/// and it exists only when that env flag is set: a daemon nobody asked to
/// simulate anything never emits one.
struct ForcedBench {
    /// The model whose simulated benchmark missed the latency duty.
    from_model: String,
    /// The smaller model it stepped down to, or `None` when nothing smaller
    /// clears the duty and the tier is disabled instead.
    to_model: Option<String>,
}

/// Run the first-run hardware probe against `profile`.
///
/// The profile and catalog are passed in rather than resolved here so the probe
/// and the REQ-547 consent gate describe the *same* machine and the *same*
/// catalog — re-detecting would let the two disagree.
fn probe_local_tier(
    profile: &HardwareProfile,
    catalog: &Catalog,
    pinned: Option<&str>,
) -> ProbeResult {
    let decision = decide(profile, catalog, pinned);
    let above_floor = profile.ram_bytes >= 8 * GIB;

    match decision {
        TierDecision::Disabled { reason } => ProbeResult {
            model: None,
            probed_model: None,
            disabled: true,
            disabled_reason: Some(reason),
            ram_bytes: profile.ram_bytes,
            above_floor,
            forced_bench: None,
        },
        TierDecision::Selected { model, .. } => {
            // A forced-slow micro-benchmark trips the BR-8 latency duty and
            // auto-steps-down to the next smaller catalog model (AC-8). It
            // publishes `benchmark` and `stepped_down` stages for measurements
            // that never happened, so it is a test seam like the rest (E-6) and
            // is honoured only under the master switch: a shipped daemon must not
            // be able to be told to narrate work it did not do.
            if env_flag("TETON_PROBE_FORCE_SLOW_BENCH") && test_seams_enabled() {
                let to_model = step_down_target(catalog, &model);
                return ProbeResult {
                    model: to_model.clone(),
                    probed_model: Some(model.clone()),
                    disabled: to_model.is_none(),
                    disabled_reason: to_model.is_none().then(|| {
                        "no smaller model clears the latency duty; remote-only".to_owned()
                    }),
                    ram_bytes: profile.ram_bytes,
                    above_floor,
                    forced_bench: Some(ForcedBench {
                        from_model: model,
                        to_model,
                    }),
                };
            }

            ProbeResult {
                model: Some(model.clone()),
                probed_model: Some(model),
                disabled: false,
                disabled_reason: None,
                ram_bytes: profile.ram_bytes,
                above_floor,
                forced_bench: None,
            }
        }
    }
}

/// The startup `model_lifecycle` sequence replayed to every attaching client.
///
/// **Every stage here is a claim about something that actually happened.** The
/// sequence this replaced announced `download …`, `benchmark …` and `local model
/// … ready` on every attach — before the user had answered the proposal, and on
/// a machine with no weights at all. In a daemon whose thesis is legibility that
/// is worse than saying nothing: a client cannot distinguish a real readiness
/// from a decorative one, so the honest states have to be nameable.
///
/// What this daemon can truthfully say at startup:
///
/// | State | Stage |
/// |---|---|
/// | the probe ran | `probed` (always) |
/// | below the floor / no fitting entry | `disabled`, with the probe's reason |
/// | a proposal is open, or weights are missing | `awaiting_decision` |
/// | the tier was declined (BR-4) | `disabled`, saying so |
/// | weights installed, nothing in this build can load them | `disabled`, saying so |
/// | an engine is loaded and serving | `ready` |
///
/// Nothing here claims a download: the only `download` stages that reach a
/// client come from [`crate::install::LifecycleProgress`], which publishes bytes
/// as they actually move.
fn startup_lifecycle(
    probe: &ProbeResult,
    engine_present: bool,
    consent: &ModelConsentGate,
) -> Vec<ModelLifecycle> {
    let model_id = probe
        .model
        .clone()
        .unwrap_or_else(|| LOCAL_TIER_ID.to_owned());
    let mut lifecycle = vec![ModelLifecycle {
        // The model the *probe* chose, which a simulated step-down may since have
        // moved off.
        model_id: probe
            .probed_model
            .clone()
            .unwrap_or_else(|| LOCAL_TIER_ID.to_owned()),
        stage: ModelLifecycleStage::Probed {
            ram_bytes: probe.ram_bytes,
            above_floor: probe.above_floor,
        },
    }];

    // The explicitly-requested simulation, and only when requested.
    if let Some(bench) = &probe.forced_bench {
        lifecycle.push(ModelLifecycle {
            model_id: bench.from_model.clone(),
            stage: ModelLifecycleStage::Benchmark {
                first_token_ms: 2_500,
                tokens_per_sec: 2.0,
            },
        });
        if let Some(to_model) = &bench.to_model {
            lifecycle.push(ModelLifecycle {
                model_id: bench.from_model.clone(),
                stage: ModelLifecycleStage::SteppedDown {
                    from_model: bench.from_model.clone(),
                    to_model: to_model.clone(),
                    reason: "benchmark exceeded the 1s first-token latency duty".to_owned(),
                },
            });
            lifecycle.push(ModelLifecycle {
                model_id: to_model.clone(),
                stage: ModelLifecycleStage::Benchmark {
                    first_token_ms: 600,
                    tokens_per_sec: 30.0,
                },
            });
        }
    }

    if probe.disabled {
        lifecycle.push(ModelLifecycle {
            model_id,
            stage: ModelLifecycleStage::Disabled {
                reason: probe
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| "the local tier is unavailable on this machine".to_owned()),
            },
        });
        return lifecycle;
    }

    // An engine is loaded and answering: `ready` is a fact, not a hope. Today
    // that means a `TETON_LOCAL_SCRIPT` engine — which downloads nothing, which
    // is exactly why it is not gated on a consent decision.
    if engine_present {
        lifecycle.push(ModelLifecycle {
            model_id,
            stage: ModelLifecycleStage::Ready,
        });
        return lifecycle;
    }

    let declined = consent
        .current_selection()
        .is_some_and(|selection| selection.declined_local);
    let stage = if declined {
        // BR-4: a settled, deliberate absence. Not a failure and not a prompt.
        ModelLifecycleStage::Disabled {
            reason: "the local tier was declined; sessions run remote-only. \
                     `teton model set <name>` changes that."
                .to_owned(),
        }
    } else if consent.consent_required() {
        // BR-1: proposed and unanswered, or answered but the weights are gone.
        // Nothing has been fetched, measured, or loaded, and the sequence says so.
        ModelLifecycleStage::AwaitingDecision {
            reason: "proposed for this machine — nothing is downloaded, benchmarked, or loaded \
                     until you answer; sessions run remote-only until then."
                .to_owned(),
        }
    } else {
        // Decided, downloaded, verified — and unloadable, because nothing in this
        // build constructs a local engine from installed weights (REQ-544 debt,
        // tracked as AC-2). Saying `ready` here would be the exact untruth this
        // function exists to stop. The reason is shared with the consent gate's
        // install-time event (M-1) so the two can never drift apart.
        ModelLifecycleStage::Disabled {
            reason: no_local_engine_reason(&model_id),
        }
    };
    lifecycle.push(ModelLifecycle { model_id, stage });
    lifecycle
}

/// The hardware profile to probe: env overrides when present, else detected.
///
/// DECISION 3 / E-6: the overrides are test seams like every other, honoured only
/// under [`test_seams_enabled`]. They were the three ungated ones, and they were
/// the worst three to leave open: `ram_bytes` feeds [`validate_choice`], so a
/// `TETON_PROBE_RAM_BYTES` large enough would make every catalog entry look like
/// it fits and suppress BR-3's above-the-floor confirmation outright — while the
/// "hardware" figures the consent screen shows the user came from the environment
/// rather than the machine. A shipped daemon describes the machine it is on.
///
/// [`validate_choice`]: crate::model_consent::validate_choice
fn probe_profile() -> HardwareProfile {
    let seams = test_seams_enabled();
    let ram = env_u64("TETON_PROBE_RAM_BYTES").filter(|_| seams);
    let disk = env_u64("TETON_PROBE_DISK_BYTES").filter(|_| seams);
    let gpu = std::env::var("TETON_PROBE_GPU").ok().filter(|_| seams);
    if !seams
        && (std::env::var_os("TETON_PROBE_RAM_BYTES").is_some()
            || std::env::var_os("TETON_PROBE_DISK_BYTES").is_some()
            || std::env::var_os("TETON_PROBE_GPU").is_some())
    {
        eprintln!(
            "tetond: ignoring TETON_PROBE_RAM_BYTES/_DISK_BYTES/_GPU — they are test seams \
             honoured only in a debug build with TETON_TEST_SEAMS=1, not operator overrides. \
             Probing the real machine."
        );
    }
    if ram.is_some() || disk.is_some() || gpu.is_some() {
        return HardwareProfile {
            ram_bytes: ram.unwrap_or(16 * GIB),
            free_disk_bytes: disk.unwrap_or(500_000 * 1_000_000),
            gpu: match gpu.as_deref() {
                Some("apple-silicon") => GpuClass::AppleSilicon,
                Some("cuda") => GpuClass::Cuda,
                _ => GpuClass::Cpu,
            },
        };
    }
    HardwareProfile::detect().unwrap_or(HardwareProfile {
        ram_bytes: 16 * GIB,
        free_disk_bytes: 500_000 * 1_000_000,
        gpu: GpuClass::Cpu,
    })
}

/// The next-smaller catalog model to step down to (by descending download size).
fn step_down_target(catalog: &Catalog, current: &str) -> Option<String> {
    let current_entry = catalog.get(current)?;
    catalog
        .models
        .iter()
        .filter(|e| e.size_bytes < current_entry.size_bytes)
        .max_by_key(|e| e.size_bytes)
        .map(|e| e.name.clone())
}

/// Whether the local tier starts out **withheld** pending a decision (BR-1 / E-5).
///
/// Two inputs, one rule: the tier is withheld while a consent decision is
/// outstanding, and the *only* exemption is a scripted engine — canned replies
/// from a file, which download nothing, so there is nothing to consent to.
///
/// Named and separated because the expression used to be
/// `engine.is_none() && consent.consent_required()`, which is the same thing only
/// while the scripted engine is the *sole* engine this build can construct. A
/// real weights-loading engine is not an exemption; it is precisely the case the
/// gate exists for, and the old spelling would have opened the tier for it
/// unconditionally — while `first_run_consent_applies()`, keyed the same way,
/// stopped the consent flow (and its deep verification) from ever running.
fn local_tier_gated(scripted_engine: bool, consent_required: bool) -> bool {
    consent_required && !scripted_engine
}

/// A constructed local engine, and what kind of engine it is (E-5).
///
/// The kind travels with the engine because the consent flow's one exemption is
/// about the *kind* — a scripted engine downloads nothing — and inferring it from
/// "an engine exists" silently becomes wrong the day a real GGUF loader lands.
struct LocalEngine {
    /// The engine the router will call.
    engine: Arc<Mutex<dyn Engine>>,
    /// Whether it replays canned replies from `TETON_LOCAL_SCRIPT` rather than
    /// loading weights the daemon would have had to download.
    scripted: bool,
}

/// Build the local engine when a scripted engine is configured and the probe did
/// not disable the tier.
///
/// A real weights-loading engine would be constructed here too, and would carry
/// `scripted: false` — which is what would (correctly) keep the consent flow and
/// its deep verification switched on for it.
fn build_local_engine(probe: &ProbeResult) -> Option<LocalEngine> {
    if probe.disabled {
        return None;
    }
    let script = std::env::var_os("TETON_LOCAL_SCRIPT")?;
    let model_id = probe
        .model
        .clone()
        .unwrap_or_else(|| "scripted-local".to_owned());
    let engine = ScriptedFileEngine::from_file(model_id, Path::new(&script)).ok()?;
    Some(LocalEngine {
        engine: Arc::new(Mutex::new(engine)) as Arc<Mutex<dyn Engine>>,
        scripted: true,
    })
}

/// The Anthropic Messages API version header value the credential layer injects
/// alongside `x-api-key` (mirrors the adapter's protocol header; the injected
/// copy wins so no duplicate reaches the wire).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Build the endpoint-bound HTTP transport for a remote provider turn (BR-7,
/// REQ-544 M-3).
///
/// A provider with no `auth_ref` gets a credential-free transport (the e2e mock
/// endpoints and any keyless provider). Otherwise the `auth_ref` is resolved to a
/// secret, turned into the provider-appropriate credential header(s), and bound
/// to the provider's endpoint origin so the header can never ride an MCP or
/// cross-provider request. A resolution failure is a typed
/// [`HarnessError::Credential`] — never a panic, and its message never carries
/// the secret.
fn build_remote_transport(
    provider: &ModelProvider,
    resolver: &SecretResolver,
) -> Result<HttpTransport, HarnessError> {
    match provider.auth_ref.as_deref() {
        None => HttpTransport::new()
            .map_err(|e| HarnessError::Engine(EngineError::Backend(e.to_string()))),
        Some(auth_ref) => {
            // BR-7 / REQ-544 M-3: an `auth_ref` provider MUST have an endpoint that
            // parses to a tuple (network-addressable) origin, or the resolved
            // credential can never be bound to it — `with_endpoint_auth` would
            // attach the header to nothing (`origin_of` is `None`), the call would
            // 401, and there would be no sign the auth was silently stripped.
            // Reject it loudly as a config/credential error instead. Checked before
            // the keychain is touched, and the message names only the reference
            // (config, safe) — never the secret.
            let endpoint = provider.endpoint.clone().unwrap_or_default();
            if origin_of(&endpoint).is_none() {
                return Err(HarnessError::Credential(format!(
                    "provider `{}` declares auth_ref `{auth_ref}` but its endpoint does not \
                     parse to a network origin; the credential cannot be bound and would be \
                     silently dropped",
                    provider.id
                )));
            }
            let secret = resolver
                .resolve(auth_ref)
                .map_err(|e| HarnessError::Credential(e.to_string()))?;
            let headers = provider_auth_headers(provider.kind, &secret);
            HttpTransport::with_endpoint_auth(&endpoint, headers)
                .map_err(|e| HarnessError::Engine(EngineError::Backend(e.to_string())))
        }
    }
}

/// The provider-appropriate credential header(s) for a resolved `secret` (BR-7).
///
/// Anthropic authenticates with `x-api-key` (plus the `anthropic-version` the
/// API requires); OpenAI-compatible and custom endpoints use a bearer token. The
/// local tier never authenticates. Header *names* are safe to construct here; the
/// secret value lives only in the returned headers and is dropped after the
/// endpoint-bound transport is built — it never reaches a log or `CostRecord`.
fn provider_auth_headers(kind: ProviderKind, secret: &str) -> Vec<(String, String)> {
    match kind {
        ProviderKind::Anthropic => vec![
            ("x-api-key".to_owned(), secret.to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
        ],
        ProviderKind::OpenaiCompatible | ProviderKind::Custom => {
            vec![("authorization".to_owned(), format!("Bearer {secret}"))]
        }
        // The local tier does not reach a remote transport, so it needs no auth.
        ProviderKind::Local => Vec::new(),
    }
}

/// Build a concrete [`Provider`] adapter from a config provider entry.
fn build_provider(provider: &ModelProvider, caps: CapabilityProfile) -> Box<dyn Provider> {
    let endpoint = provider.endpoint.clone().unwrap_or_default();
    match provider.kind {
        ProviderKind::Anthropic => {
            Box::new(AnthropicAdapter::new(provider.id.clone(), endpoint).with_capabilities(caps))
        }
        // OpenAI-compatible and custom both speak the OpenAI chat/completions
        // shape in the MVP.
        _ => Box::new(OpenAiCompatAdapter::new(
            OpenAiCompatConfig::new(provider.id.clone(), endpoint).with_capabilities(caps),
        )),
    }
}

/// Build the phase-policy [`Router`] from a config snapshot.
///
/// Each provider's billed model name is resolved from the price table
/// ([`billing_model`]) so cost attribution (BR-2) hits a real price entry rather
/// than the bare provider id.
fn build_router(
    config: &Config,
    local_available: bool,
    prices: &PriceTable,
    health: &BTreeMap<String, ProviderHealth>,
) -> Router {
    let local_provider = config
        .providers
        .iter()
        .find(|p| matches!(p.kind, ProviderKind::Local))
        .map_or_else(|| "local".to_owned(), |p| p.id.clone());
    let default_provider = config
        .providers
        .iter()
        .find(|p| p.kind.is_remote())
        .map_or_else(|| local_provider.clone(), |p| p.id.clone());

    let mut router = Router::new(config.routing.clone(), default_provider, local_provider)
        .with_local_available(local_available);
    for p in &config.providers {
        // REQ-544 M-5: seed each provider's health from the persisted map (default
        // Healthy for a provider never observed failing). This is the read side of
        // the cross-turn health fallback — a provider marked Unavailable last turn
        // is seeded Unavailable now, so policy evaluation fails over to the fallback.
        let seed = health
            .get(&p.id)
            .copied()
            .unwrap_or(ProviderHealth::Healthy);
        router = router.with_provider(
            p.id.clone(),
            billing_model(prices, &p.id),
            CapabilityProfile::from_core(p.capabilities),
            seed,
        );
    }
    router
}

/// The cross-turn health a provider should carry after a failure of `class`
/// (REQ-544 M-5). A persistent failure (fallback / fail) marks it `Unavailable`
/// so the next turn's policy evaluation fails over; a weak-tool-calling failure
/// marks it `Degraded` (kept, reduced profile); a transient failure leaves health
/// unchanged (`None`) so a retryable blip does not strand a provider.
fn health_after_failure(class: FailureClass) -> Option<ProviderHealth> {
    match classify(class).action {
        FailureAction::Fallback | FailureAction::Fail => Some(ProviderHealth::Unavailable),
        FailureAction::Degrade => Some(ProviderHealth::Degraded),
        FailureAction::Retry => None,
    }
}

/// The half-open cooldown a provider marked `Unavailable` by `class` should carry
/// (REQ-544 M-5). An auth-shaped client error (401/403) recovers on the shorter
/// [`PROVIDER_AUTH_COOLDOWN`] — an operator-fixed credential should be re-probed
/// sooner — while every other persistent failure uses the default
/// [`PROVIDER_UNAVAILABLE_COOLDOWN`].
fn cooldown_for(class: FailureClass) -> Duration {
    match class {
        FailureClass::ClientError { status: 401 | 403 } => PROVIDER_AUTH_COOLDOWN,
        _ => PROVIDER_UNAVAILABLE_COOLDOWN,
    }
}

/// The persisted [`HealthRecord`] a provider should carry after a failure of
/// `class` at `now` (REQ-544 M-5). Layers the half-open cooldown ([`cooldown_for`])
/// onto the health decision ([`health_after_failure`]): a persistent failure
/// becomes `Unavailable` with a recovery deadline, a weak-tool-calling failure
/// degrades (no deadline — kept in rotation), and a transient failure records
/// nothing (`None`).
fn health_record_after_failure(class: FailureClass, now: Instant) -> Option<HealthRecord> {
    match health_after_failure(class)? {
        ProviderHealth::Unavailable => Some(HealthRecord::unavailable(now, cooldown_for(class))),
        ProviderHealth::Degraded => Some(HealthRecord::degraded()),
        // `health_after_failure` only ever yields Unavailable/Degraded/None; a
        // Healthy downgrade is not a thing.
        ProviderHealth::Healthy => Some(HealthRecord::healthy()),
    }
}

/// Whether the assembled context in `ctx` carries content that must pin the
/// session to the local tier (REQ-544 C-2): its egress provenance intersects a
/// `local-only` boundary, or it carries unknown provenance (a `shell` result).
///
/// With no boundaries configured, nothing is sensitive — there is nothing to
/// protect. Boundaries that fail to compile fail-closed (treated as sensitive),
/// the same posture the egress choke point takes.
fn context_is_sensitive(ctx: &ContextManager, boundaries: &[PrivacyBoundary]) -> bool {
    if boundaries.is_empty() {
        return false;
    }
    let provenance = context_provenance(ctx);
    if provenance.is_empty() {
        return false;
    }
    match BoundaryMatcher::new(boundaries) {
        Ok(matcher) => inspect(&provenance, &matcher, PrivacyAction::ReroutedToLocal).is_blocked(),
        Err(_) => true,
    }
}

/// The model name a provider is billed under: the first price-table entry for
/// that provider id, or the provider id itself when the table knows no model for
/// it (an unpriced provider, recorded but never guessed a cost, BR-2).
fn billing_model(prices: &PriceTable, provider_id: &str) -> String {
    prices
        .models
        .iter()
        .find(|m| m.provider_id == provider_id)
        .map_or_else(|| provider_id.to_owned(), |m| m.model.clone())
}

// ---------------------------------------------------------------------------
// Config <-> protocol conversions
// ---------------------------------------------------------------------------

/// Project a [`Config`] into the protocol [`ConfigSnapshot`] for `config/get`.
fn snapshot_from_config(config: &Config) -> ConfigSnapshot {
    ConfigSnapshot {
        providers: config
            .providers
            .iter()
            .map(|p| ProviderConfig {
                id: ProviderId::from(p.id.as_str()),
                kind: to_proto_kind(p.kind),
                endpoint: p.endpoint.clone(),
                auth_ref: p.auth_ref.clone(),
            })
            .collect(),
        routing: config
            .routing
            .iter()
            .map(|r| RoutingRule {
                phase: to_proto_phase(r.phase),
                provider_id: ProviderId::from(r.provider_id.as_str()),
                fallback_id: r.fallback_id.as_deref().map(ProviderId::from),
            })
            .collect(),
        privacy: config
            .boundaries
            .iter()
            .map(|b| PrivacyBoundaryConfig {
                path_glob: b.path_glob.clone(),
                mode: to_proto_mode(b.mode),
            })
            .collect(),
    }
}

/// Apply a single [`ConfigUpdate`] to `config` in place (replace-or-insert).
fn apply_update(config: &mut Config, update: ConfigUpdate) {
    match update {
        ConfigUpdate::RegisterProvider(pc) => {
            let provider = ModelProvider {
                id: pc.id.0,
                kind: to_core_kind(pc.kind),
                endpoint: pc.endpoint,
                auth_ref: pc.auth_ref,
                capabilities: ProviderCapabilities::default(),
            };
            if let Some(existing) = config.providers.iter_mut().find(|p| p.id == provider.id) {
                *existing = provider;
            } else {
                config.providers.push(provider);
            }
        }
        ConfigUpdate::SetRoutingRule(rr) => {
            let rule = RoutingPolicy {
                phase: to_core_phase(rr.phase),
                provider_id: rr.provider_id.0,
                fallback_id: rr.fallback_id.map(|f| f.0),
            };
            if let Some(existing) = config.routing.iter_mut().find(|r| r.phase == rule.phase) {
                *existing = rule;
            } else {
                config.routing.push(rule);
            }
        }
        ConfigUpdate::SetPrivacyBoundary(pb) => {
            let boundary = PrivacyBoundary {
                path_glob: pb.path_glob,
                mode: to_core_mode(pb.mode),
            };
            if let Some(existing) = config
                .boundaries
                .iter_mut()
                .find(|b| b.path_glob == boundary.path_glob)
            {
                *existing = boundary;
            } else {
                config.boundaries.push(boundary);
            }
        }
    }
}

/// Project the daemon's cost report into the wire [`CostReportView`].
fn cost_report_view(report: &CostReport) -> CostReportView {
    let group = |g: &GroupTotals| CostGroupView {
        key: g.key.clone(),
        calls: g.calls,
        input_tokens: g.input_tokens,
        output_tokens: g.output_tokens,
        usd_micros: g.usd_micros,
    };
    CostReportView {
        total_usd_micros: report.total.usd_micros,
        total_calls: report.total.calls,
        priced_calls: report.total.priced_calls,
        unpriced_calls: report.total.unpriced_calls,
        savings_usd_micros: report.savings.savings_usd_micros,
        baseline_usd_micros: report.savings.baseline_usd_micros,
        baseline_model: report.savings.baseline_model.clone(),
        methodology: report.methodology.clone(),
        per_phase: report.per_phase.iter().map(group).collect(),
        per_provider: report.per_provider.iter().map(group).collect(),
    }
}

fn to_proto_kind(kind: ProviderKind) -> ProtoProviderKind {
    match kind {
        ProviderKind::Local => ProtoProviderKind::Local,
        ProviderKind::OpenaiCompatible => ProtoProviderKind::OpenaiCompatible,
        ProviderKind::Anthropic => ProtoProviderKind::Anthropic,
        ProviderKind::Custom => ProtoProviderKind::Custom,
    }
}

fn to_core_kind(kind: ProtoProviderKind) -> ProviderKind {
    match kind {
        ProtoProviderKind::Local => ProviderKind::Local,
        ProtoProviderKind::OpenaiCompatible => ProviderKind::OpenaiCompatible,
        ProtoProviderKind::Anthropic => ProviderKind::Anthropic,
        ProtoProviderKind::Custom => ProviderKind::Custom,
    }
}

fn to_proto_phase(phase: CorePhase) -> ProtoPhase {
    match phase {
        CorePhase::Spec => ProtoPhase::Spec,
        CorePhase::Architect => ProtoPhase::Architect,
        CorePhase::Implement => ProtoPhase::Implement,
        CorePhase::Review => ProtoPhase::Review,
        CorePhase::Io => ProtoPhase::Io,
        CorePhase::Freeform => ProtoPhase::Freeform,
    }
}

fn to_core_phase(phase: ProtoPhase) -> CorePhase {
    match phase {
        ProtoPhase::Spec => CorePhase::Spec,
        ProtoPhase::Architect => CorePhase::Architect,
        ProtoPhase::Implement => CorePhase::Implement,
        ProtoPhase::Review => CorePhase::Review,
        ProtoPhase::Io => CorePhase::Io,
        ProtoPhase::Freeform => CorePhase::Freeform,
    }
}

fn to_proto_mode(mode: BoundaryMode) -> PrivacyMode {
    match mode {
        BoundaryMode::LocalOnly => PrivacyMode::LocalOnly,
        BoundaryMode::RedactThenRemote => PrivacyMode::RedactThenRemote,
    }
}

fn to_core_mode(mode: PrivacyMode) -> BoundaryMode {
    match mode {
        PrivacyMode::LocalOnly => BoundaryMode::LocalOnly,
        PrivacyMode::RedactThenRemote => BoundaryMode::RedactThenRemote,
    }
}

/// Read an env var as a `u64`, returning `None` when unset or unparsable.
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Whether an env flag is set to a truthy value.
fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1" | "true" | "yes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory under the system temp dir, unique per test.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-loadcfg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_absent_config_file_defaults() {
        // A fresh install has no config; defaulting there is correct.
        let dir = scratch_dir("absent");
        let missing = dir.join("config.toml");
        assert_eq!(
            load_config(Some(&missing)).expect("an absent file defaults"),
            Config::default()
        );
        // No path at all also defaults.
        assert_eq!(
            load_config(None).expect("no path defaults"),
            Config::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_present_but_invalid_config_refuses_rather_than_dropping_boundaries() {
        // H-1: a config that EXISTS but fails validation must NOT be silently
        // replaced by `Config::default()` (which has `boundaries: vec![]`). Here a
        // one-character mistake — a `base_url` with no scheme — sits beside a
        // declared `local-only` privacy boundary. Failing open would drop that
        // boundary on the floor with nothing logged; instead the load refuses.
        let dir = scratch_dir("invalid");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[local_model]\nbase_url = \"hf-mirror.corp.internal\"\n\n\
             [[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n",
        )
        .unwrap();

        let err = load_config(Some(&path))
            .expect_err("a present-but-invalid config must refuse, not fail open");
        let message = err.to_string();
        // The refusal explains itself and names the offending field's rule, so an
        // operator can fix it rather than unknowingly running with no boundaries.
        assert!(
            message.contains("invalid") && message.contains("boundaries"),
            "diagnostic should explain the fail-open it prevented: {message}"
        );

        // The proof it did not fail open: the very same file, with only the
        // base_url corrected, loads AND still carries the privacy boundary. So the
        // refusal above was the invalidity, never a dropped boundary.
        std::fs::write(
            &path,
            "[local_model]\nbase_url = \"https://hf-mirror.corp.internal\"\n\n\
             [[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n",
        )
        .unwrap();
        let loaded = load_config(Some(&path)).expect("the corrected config loads");
        assert_eq!(
            loaded.boundaries.len(),
            1,
            "a valid config keeps its declared privacy boundaries"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_with_the_deprecated_legacy_pin_refuses_to_start() {
        // Decision 2 + H-1 together: the legacy `pinned_local_model` key now fails
        // validation, and `load_config` surfaces that as a refusal rather than
        // defaulting past it.
        let dir = scratch_dir("legacy-pin");
        let path = dir.join("config.toml");
        std::fs::write(&path, "pinned_local_model = \"qwen2.5-coder-3b\"\n").unwrap();
        let err = load_config(Some(&path)).expect_err("a deprecated legacy pin must refuse");
        assert!(err.to_string().contains("invalid"), "diagnostic: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripted_engine_replays_blocks_then_ends() {
        let script = "first reply\n---\nsecond reply\n---\nthird";
        let engine = ScriptedFileEngine::from_script("m", script);
        let params = GenParams::default();
        let mut sink = |_: &str| {};
        assert_eq!(
            engine.complete("p", &params, &mut sink).unwrap().text,
            "first reply"
        );
        assert_eq!(
            engine.complete("p", &params, &mut sink).unwrap().text,
            "second reply"
        );
        assert_eq!(
            engine.complete("p", &params, &mut sink).unwrap().text,
            "third"
        );
        // Exhausted: a plain-text end-of-turn so no loop can outrun the mock.
        assert_eq!(
            engine.complete("p", &params, &mut sink).unwrap().text,
            "Done."
        );
    }

    #[test]
    fn last_tool_result_body_extracts_the_most_recent_tool_block() {
        // The flat rendering shape the local engine is handed.
        let prompt = "SYSTEM\n\nUser:\ndo it\n\nAssistant:\n{\"tool\":\"read\"}\n\n\
                      Tool (read):\nfirst file body\n\nAssistant:\n\
                      {\"tool\":\"mcp__demo__echo\"}\n\n\
                      Tool (mcp__demo__echo):\nechoed from the demo MCP server\n\nAssistant:\n";
        assert_eq!(
            last_tool_result_body(prompt),
            "echoed from the demo MCP server"
        );
        // No tool block at all → empty (the regression signal).
        assert_eq!(
            last_tool_result_body("SYSTEM\n\nUser:\nhi\n\nAssistant:\n"),
            ""
        );
    }

    #[test]
    fn scripted_reply_substitutes_the_real_tool_result() {
        // REQ-544 AC-9 execution proof: a reply that quotes {{LAST_TOOL_RESULT}}
        // reflects the tool output actually present in the prompt, so discarding
        // the result would change the reply.
        let engine =
            ScriptedFileEngine::from_script("m", "Done. The tool said: {{LAST_TOOL_RESULT}}");
        let params = GenParams::default();
        let mut sink = |_: &str| {};
        let prompt =
            "SYSTEM\n\nTool (mcp__demo__echo):\nechoed from the demo MCP server\n\nAssistant:\n";
        let out = engine.complete(prompt, &params, &mut sink).unwrap().text;
        assert_eq!(out, "Done. The tool said: echoed from the demo MCP server");

        // With no tool result in context the placeholder resolves to empty — the
        // sentinel is gone, which is exactly what fails the AC-9 assertion under a
        // plumbing regression.
        let engine2 =
            ScriptedFileEngine::from_script("m", "Done. The tool said: {{LAST_TOOL_RESULT}}");
        let bare = engine2
            .complete("SYSTEM\n\nAssistant:\n", &params, &mut sink)
            .unwrap()
            .text;
        assert_eq!(bare, "Done. The tool said: ");
        assert!(!bare.contains("echoed from the demo MCP server"));
    }

    #[test]
    fn config_snapshot_round_trips_kinds_and_modes() {
        let mut config = Config::default();
        apply_update(
            &mut config,
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("deepseek"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com/v1/chat/completions".to_owned()),
                auth_ref: Some("keychain:deepseek".to_owned()),
            }),
        );
        apply_update(
            &mut config,
            ConfigUpdate::SetRoutingRule(RoutingRule {
                phase: ProtoPhase::Implement,
                provider_id: ProviderId::from("deepseek"),
                fallback_id: None,
            }),
        );
        apply_update(
            &mut config,
            ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
                path_glob: "secrets/**".to_owned(),
                mode: PrivacyMode::LocalOnly,
            }),
        );
        config.validate().expect("valid");

        let snap = snapshot_from_config(&config);
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.providers[0].kind, ProtoProviderKind::OpenaiCompatible);
        assert_eq!(snap.routing[0].phase, ProtoPhase::Implement);
        assert_eq!(snap.privacy[0].mode, PrivacyMode::LocalOnly);
    }

    #[test]
    fn apply_update_replaces_rather_than_duplicates() {
        let mut config = Config::default();
        let register = |endpoint: &str| {
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("p"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(endpoint.to_owned()),
                auth_ref: None,
            })
        };
        apply_update(
            &mut config,
            register("https://a.example/v1/chat/completions"),
        );
        apply_update(
            &mut config,
            register("https://b.example/v1/chat/completions"),
        );
        assert_eq!(config.providers.len(), 1);
        assert_eq!(
            config.providers[0].endpoint.as_deref(),
            Some("https://b.example/v1/chat/completions")
        );
    }

    /// E-5: the consent gate must not switch itself off the moment a real engine
    /// appears — which is exactly when downloading weights starts to mean
    /// something.
    #[test]
    fn only_a_scripted_engine_exempts_the_local_tier_from_the_consent_gate() {
        // The ordinary first run on a production build: withheld until answered.
        assert!(local_tier_gated(false, true));
        // Decided and installed: open.
        assert!(!local_tier_gated(false, false));
        // A `TETON_LOCAL_SCRIPT` engine fetches nothing, so it is never gated.
        assert!(!local_tier_gated(true, true));
        assert!(!local_tier_gated(true, false));
        // And the regression this pins: a build that HAS a weights-loading engine
        // (`scripted_engine == false`) and an outstanding decision is withheld.
        // The old `engine.is_none() && …` spelling made that case un-gated.
        assert!(
            local_tier_gated(false, true),
            "a real engine must not un-gate the tier before the user has decided"
        );
    }

    /// DECISION 3 / E-6: the master switch is a debug-build affordance, and a
    /// release build asked to honour it must **refuse**, not quietly ignore it.
    #[test]
    fn the_seam_master_switch_is_debug_only_and_refuses_loudly_in_a_release_build() {
        assert_eq!(seam_policy(true, Some("1")), SeamPolicy::Honour);
        assert_eq!(seam_policy(true, None), SeamPolicy::Ignore);
        assert_eq!(seam_policy(true, Some("0")), SeamPolicy::Ignore);
        assert_eq!(seam_policy(true, Some("yes")), SeamPolicy::Ignore);
        // The branch a debug-build test cannot otherwise reach: whoever set this
        // believes the daemon is running against mocks, simulated hardware and a
        // capped volume. Ignoring them silently means they read a production run
        // as a test result.
        assert_eq!(seam_policy(false, Some("1")), SeamPolicy::Refuse);
        // Turning the seams off explicitly is not a mistake to refuse over.
        assert_eq!(seam_policy(false, Some("0")), SeamPolicy::Ignore);
        assert_eq!(seam_policy(false, None), SeamPolicy::Ignore);
    }

    #[test]
    fn below_floor_probe_disables_the_local_tier() {
        let profile = HardwareProfile {
            ram_bytes: 4 * GIB,
            free_disk_bytes: 500_000 * 1_000_000,
            gpu: GpuClass::AppleSilicon,
        };
        let catalog = Catalog::bundled();
        let decision = decide(&profile, &catalog, None);
        assert!(decision.is_disabled());
    }

    #[test]
    fn session_taint_pins_a_session_idempotently() {
        // REQ-544 C-2: once marked, a session stays tainted; other sessions are
        // unaffected.
        let taint = SessionTaint::new();
        let s = SessionId::from("s1");
        assert!(!taint.is_tainted(&s));
        taint.mark(&s);
        taint.mark(&s); // idempotent
        assert!(taint.is_tainted(&s));
        assert!(!taint.is_tainted(&SessionId::from("other")));
    }

    #[test]
    fn context_sensitivity_flags_boundary_and_unknown_but_not_public() {
        use crate::harness::context::ToolProvenance;
        let boundaries = vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }];

        // A read of a boundary file taints (REQ-544 C-2).
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_tool_result("read", Some("secrets/prod.env".to_owned()), "API_KEY=1");
        assert!(context_is_sensitive(&ctx, &boundaries));

        // An unknown-provenance shell result taints even with no boundary path.
        let mut ctx_shell = ContextManager::new("sys", 10_000);
        ctx_shell.push_tool_result_prov("shell", ToolProvenance::Unknown, "cmd output");
        assert!(context_is_sensitive(&ctx_shell, &boundaries));

        // A public-only context does not taint.
        let mut ctx_public = ContextManager::new("sys", 10_000);
        ctx_public.push_tool_result("read", Some("src/lib.rs".to_owned()), "code");
        assert!(!context_is_sensitive(&ctx_public, &boundaries));

        // With no boundaries configured, nothing is sensitive.
        assert!(!context_is_sensitive(&ctx, &[]));
    }

    // --- REQ-544 M-3: endpoint-bound credential injection ------------------

    use crate::keychain::{BackendError, KeychainBackend};

    /// A keychain fake for the runtime tests — returns a canned secret so no
    /// test touches the real OS keychain.
    struct FakeBackend {
        secret: String,
    }

    impl KeychainBackend for FakeBackend {
        fn get(&self, _service: &str, _account: &str) -> Result<String, BackendError> {
            Ok(self.secret.clone())
        }
    }

    fn resolver_returning(secret: &str) -> SecretResolver {
        SecretResolver::with_backend(Box::new(FakeBackend {
            secret: secret.to_owned(),
        }))
    }

    fn provider(kind: ProviderKind, endpoint: &str, auth_ref: Option<&str>) -> ModelProvider {
        ModelProvider {
            id: "p".to_owned(),
            kind,
            endpoint: Some(endpoint.to_owned()),
            auth_ref: auth_ref.map(str::to_owned),
            capabilities: ProviderCapabilities::default(),
        }
    }

    #[test]
    fn anthropic_auth_headers_carry_the_api_key_and_version() {
        let headers = provider_auth_headers(ProviderKind::Anthropic, "sk-ant-SECRET");
        assert!(headers
            .iter()
            .any(|(n, v)| n == "x-api-key" && v == "sk-ant-SECRET"));
        assert!(headers.iter().any(|(n, _)| n == "anthropic-version"));
        // Never a bearer token for Anthropic.
        assert!(!headers.iter().any(|(n, _)| n == "authorization"));
    }

    #[test]
    fn openai_compatible_auth_uses_a_bearer_token() {
        let headers = provider_auth_headers(ProviderKind::OpenaiCompatible, "sk-deepseek");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "authorization");
        assert_eq!(headers[0].1, "Bearer sk-deepseek");
    }

    #[test]
    fn the_local_tier_carries_no_credential() {
        assert!(provider_auth_headers(ProviderKind::Local, "anything").is_empty());
    }

    #[test]
    fn a_resolved_credential_binds_only_to_the_provider_endpoint() {
        // REQ-544 M-3 end to end (network-free): resolve an auth_ref, build the
        // endpoint-bound transport, and prove the credential rides the owning
        // endpoint but never an MCP or cross-provider request.
        let endpoint = "https://api.anthropic.com/v1/messages";
        let cfg = provider(
            ProviderKind::Anthropic,
            endpoint,
            Some("keychain://teton/anthropic"),
        );
        let transport = build_remote_transport(&cfg, &resolver_returning("sk-ant-INJECTED"))
            .expect("transport");

        let owning = transport.outbound_headers(endpoint, &[]);
        assert!(owning
            .iter()
            .any(|(n, v)| n == "x-api-key" && v == "sk-ant-INJECTED"));

        let mcp = transport.outbound_headers("https://mcp.example.com/rpc", &[]);
        assert!(!mcp.iter().any(|(_, v)| v.contains("sk-ant-INJECTED")));

        let other = transport.outbound_headers("https://api.deepseek.com/v1/chat/completions", &[]);
        assert!(!other.iter().any(|(_, v)| v.contains("sk-ant-INJECTED")));
    }

    #[test]
    fn a_keyless_provider_gets_a_credential_free_transport() {
        // The e2e mock endpoints register no auth_ref; that path must still build
        // a transport, and it must inject nothing.
        let endpoint = "http://127.0.0.1:8080/v1/chat/completions";
        let cfg = provider(ProviderKind::OpenaiCompatible, endpoint, None);
        let transport = build_remote_transport(&cfg, &SecretResolver::with_default_backend())
            .expect("transport");
        let protocol = vec![("content-type".to_owned(), "application/json".to_owned())];
        assert_eq!(transport.outbound_headers(endpoint, &protocol), protocol);
    }

    #[test]
    fn an_auth_ref_provider_with_an_unparseable_endpoint_is_rejected_not_silently_stripped() {
        // REQ-544 minor: an auth_ref provider whose endpoint does not parse to a
        // network origin cannot bind its credential — `with_endpoint_auth` would
        // attach the header to nothing and the call would 401 with no sign the auth
        // was dropped. Reject it loudly (typed Credential error) instead. The
        // keychain is never touched — a resolver that would PANIC if called proves
        // the endpoint is validated first.
        struct PanicBackend;
        impl KeychainBackend for PanicBackend {
            fn get(&self, _s: &str, _a: &str) -> Result<String, BackendError> {
                panic!("the keychain must not be consulted for a broken endpoint");
            }
        }
        let resolver = SecretResolver::with_backend(Box::new(PanicBackend));

        for bad_endpoint in ["", "not-a-url", "/only/a/path", "mailto:x@y.z"] {
            let cfg = provider(
                ProviderKind::Anthropic,
                bad_endpoint,
                Some("keychain://teton/x"),
            );
            let err = build_remote_transport(&cfg, &resolver).unwrap_err();
            match err {
                HarnessError::Credential(msg) => {
                    assert!(
                        msg.contains("keychain://teton/x") && msg.contains("endpoint"),
                        "message must name the reference and the endpoint problem: {msg}"
                    );
                    assert!(!msg.contains("sk-"), "never leak a secret: {msg}");
                }
                other => panic!("expected a Credential error for `{bad_endpoint}`, got {other:?}"),
            }
        }

        // A missing endpoint (None) with an auth_ref is likewise rejected.
        let mut no_endpoint = provider(ProviderKind::Anthropic, "", Some("keychain://teton/x"));
        no_endpoint.endpoint = None;
        assert!(matches!(
            build_remote_transport(&no_endpoint, &resolver),
            Err(HarnessError::Credential(_))
        ));
    }

    #[test]
    fn an_unresolvable_credential_is_a_typed_error_not_a_panic() {
        // A missing keychain entry surfaces HarnessError::Credential whose message
        // names the reference (safe) but never the secret.
        struct MissingBackend;
        impl KeychainBackend for MissingBackend {
            fn get(&self, _s: &str, _a: &str) -> Result<String, BackendError> {
                Err(BackendError::NotFound)
            }
        }
        let cfg = provider(
            ProviderKind::Anthropic,
            "https://api.anthropic.com/v1/messages",
            Some("keychain://teton/anthropic"),
        );
        let resolver = SecretResolver::with_backend(Box::new(MissingBackend));
        let err = build_remote_transport(&cfg, &resolver).unwrap_err();
        match err {
            HarnessError::Credential(msg) => {
                assert!(msg.contains("keychain://teton/anthropic"), "{msg}");
                assert!(!msg.contains("sk-"), "{msg}");
            }
            other => panic!("expected Credential error, got {other:?}"),
        }
    }

    // --- REQ-544 M-5: cross-turn provider health ---------------------------

    /// A two-remote-provider config: Spec routes to `anthropic` with `deepseek`
    /// as the fallback — the shape that exercises the health-driven failover.
    fn two_provider_spec_config() -> Config {
        Config {
            pinned_local_model: None,
            local_model: teton_core::LocalModelConfig::default(),
            providers: vec![
                ModelProvider {
                    id: "anthropic".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com/v1/messages".to_owned()),
                    auth_ref: Some("keychain:anthropic".to_owned()),
                    capabilities: ProviderCapabilities::default(),
                },
                ModelProvider {
                    id: "deepseek".to_owned(),
                    kind: ProviderKind::OpenaiCompatible,
                    endpoint: Some("https://api.deepseek.com/v1/chat/completions".to_owned()),
                    auth_ref: Some("keychain:deepseek".to_owned()),
                    capabilities: ProviderCapabilities::default(),
                },
            ],
            routing: vec![RoutingPolicy {
                phase: CorePhase::Spec,
                provider_id: "anthropic".to_owned(),
                fallback_id: Some("deepseek".to_owned()),
            }],
            boundaries: Vec::new(),
            mcp_server: Vec::new(),
        }
    }

    #[test]
    fn a_failed_provider_is_seen_unavailable_on_the_next_turns_routing() {
        // REQ-544 M-5: provider health persists across turns. `build_router` READS
        // the daemon-wide health map, so a provider marked Unavailable after a
        // failure on one turn fails over to its fallback on the NEXT turn instead
        // of the router blindly reseeding it Healthy every turn.
        use teton_core::policy::RouteOutcome;
        let config = two_provider_spec_config();
        let prices = PriceTable::bundled();

        // Turn 1: no prior failures → the Spec primary (anthropic) is chosen.
        let fresh = BTreeMap::new();
        let route1 =
            build_router(&config, false, &prices, &fresh).resolve_structured(CorePhase::Spec);
        assert_eq!(route1.provider_id.as_ref().unwrap().0, "anthropic");
        assert_eq!(route1.outcome, RouteOutcome::Primary);

        // The primary failed with a persistent (fallback-class) error; the daemon
        // derives and records its cross-turn health.
        let downgrade = health_after_failure(FailureClass::MalformedResponse)
            .expect("a persistent failure downgrades health");
        assert_eq!(downgrade, ProviderHealth::Unavailable);
        let mut persisted = BTreeMap::new();
        persisted.insert("anthropic".to_owned(), downgrade);

        // Turn 2: build_router seeds anthropic Unavailable from the map → policy
        // fails over to the fallback deepseek. This is the cross-turn fallback that
        // was previously dead because every turn reseeded Healthy.
        let route2 =
            build_router(&config, false, &prices, &persisted).resolve_structured(CorePhase::Spec);
        assert_eq!(
            route2.provider_id.as_ref().unwrap().0,
            "deepseek",
            "a provider that failed must be seen Unavailable on the next turn's routing"
        );
        assert_eq!(route2.outcome, RouteOutcome::Fallback);
    }

    #[test]
    fn health_after_failure_only_downgrades_persistent_failures() {
        // A retryable blip must not persist as Unavailable, or a healthy provider
        // would be stranded after a single transient hiccup.
        assert!(health_after_failure(FailureClass::Timeout).is_none());
        assert!(health_after_failure(FailureClass::Transport).is_none());
        assert!(health_after_failure(FailureClass::ServerError { status: 503 }).is_none());
        // Weak tool-calling degrades (kept, reduced profile); auth / persistent
        // client errors make the provider Unavailable for the next turn.
        assert_eq!(
            health_after_failure(FailureClass::MalformedToolCall),
            Some(ProviderHealth::Degraded)
        );
        assert_eq!(
            health_after_failure(FailureClass::ClientError { status: 401 }),
            Some(ProviderHealth::Unavailable)
        );
        assert_eq!(
            health_after_failure(FailureClass::MalformedResponse),
            Some(ProviderHealth::Unavailable)
        );
    }

    #[test]
    fn an_unavailable_provider_becomes_eligible_again_after_its_cooldown() {
        // REQ-544 M-5 regression: without a half-open recovery an Unavailable
        // provider is never selected, so it can never serve a turn, so it never
        // resets to Healthy — stranded daemon-wide until restart. The cooldown
        // makes it eligible again once the window elapses. The clock is injected so
        // the test is deterministic (no real 60s sleep).
        let t0 = Instant::now();
        let cooldown = Duration::from_secs(60);
        let record = HealthRecord::unavailable(t0, cooldown);

        // Right after the failure it is still Unavailable (stranded, correctly).
        assert_eq!(record.effective_health(t0), ProviderHealth::Unavailable);
        // One second short of the deadline: still Unavailable.
        assert_eq!(
            record.effective_health(t0 + Duration::from_secs(59)),
            ProviderHealth::Unavailable
        );
        // At/after the deadline: offered as Healthy for a half-open re-probe.
        assert_eq!(
            record.effective_health(t0 + cooldown),
            ProviderHealth::Healthy
        );
        assert_eq!(
            record.effective_health(t0 + Duration::from_secs(120)),
            ProviderHealth::Healthy
        );
    }

    #[test]
    fn a_successful_reprobe_clears_a_provider_back_to_healthy() {
        // The success path records `HealthRecord::healthy()`, which is eligible at
        // any instant regardless of any prior Unavailable deadline — proving a
        // recovered provider returns to full rotation.
        let healthy = HealthRecord::healthy();
        assert_eq!(
            healthy.effective_health(Instant::now()),
            ProviderHealth::Healthy
        );
        // A degraded record is likewise always eligible (kept in rotation).
        assert_eq!(
            HealthRecord::degraded().effective_health(Instant::now()),
            ProviderHealth::Degraded
        );
    }

    #[test]
    fn an_auth_error_strands_for_a_shorter_window_than_a_malformed_response() {
        // REQ-544 M-5 "narrowed persistence": a 401 recovers sooner than a
        // malformed response, since an operator-fixed credential should re-probe
        // fast rather than be held down for the full default window.
        assert_eq!(
            cooldown_for(FailureClass::ClientError { status: 401 }),
            PROVIDER_AUTH_COOLDOWN
        );
        assert_eq!(
            cooldown_for(FailureClass::ClientError { status: 403 }),
            PROVIDER_AUTH_COOLDOWN
        );
        assert_eq!(
            cooldown_for(FailureClass::MalformedResponse),
            PROVIDER_UNAVAILABLE_COOLDOWN
        );
        assert!(
            PROVIDER_AUTH_COOLDOWN < PROVIDER_UNAVAILABLE_COOLDOWN,
            "an auth error must strand for a shorter window"
        );

        // End to end through the record builder: a 401 becomes eligible again at the
        // shorter deadline while a malformed response is still stranded there.
        let t0 = Instant::now();
        let auth = health_record_after_failure(FailureClass::ClientError { status: 401 }, t0)
            .expect("a 401 downgrades");
        let malformed = health_record_after_failure(FailureClass::MalformedResponse, t0)
            .expect("a malformed response downgrades");
        let probe_at = t0 + PROVIDER_AUTH_COOLDOWN;
        assert_eq!(auth.effective_health(probe_at), ProviderHealth::Healthy);
        assert_eq!(
            malformed.effective_health(probe_at),
            ProviderHealth::Unavailable
        );
    }

    #[test]
    fn a_transient_failure_records_no_health_downgrade() {
        // A retryable blip must not produce a HealthRecord at all (health untouched).
        assert!(health_record_after_failure(FailureClass::Timeout, Instant::now()).is_none());
        assert!(health_record_after_failure(FailureClass::Transport, Instant::now()).is_none());
        assert!(health_record_after_failure(
            FailureClass::ServerError { status: 503 },
            Instant::now()
        )
        .is_none());
        // A weak tool-calling failure degrades (kept in rotation, no deadline).
        let degraded = health_record_after_failure(FailureClass::MalformedToolCall, Instant::now())
            .expect("weak tool-calling degrades");
        assert_eq!(degraded.health, ProviderHealth::Degraded);
        assert!(degraded.retry_at.is_none());
    }
}
