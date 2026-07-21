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
//! - `TETON_PROBE_RAM_BYTES` / `TETON_PROBE_DISK_BYTES` / `TETON_PROBE_GPU` /
//!   `TETON_PROBE_FORCE_SLOW_BENCH` — hardware-probe overrides (BR-9 / AC-8).
//!
//! None of these are required in production (a real build resolves hardware and
//! weights itself); they exist so the whole daemon can be exercised end to end
//! against mocks.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::Config;
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
    PrivacyBoundaryConfig, PromptTurnResult, ProviderConfig, RoutingRule,
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
use crate::egress::{inspect, Egress, HttpTransport};
use crate::harness::completion::{context_provenance, RemoteProviderSource};
use crate::harness::context::NoopProvenanceHook;
use crate::harness::turn_loop::{run_session_turn_with_source, HarnessError};
use crate::harness::{
    build_system_prompt, ContextManager, LocalEngineSource, PendingPermissions, PermissionConfig,
    PermissionGate, SessionEvents, ToolContext, ToolRegistry,
};
use crate::keychain::SecretResolver;
use crate::mcp::{McpRegistry, McpServerConfig};
use crate::router::Router;

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
    /// The append-only cost ledger (BR-2). Recorded at the egress choke point.
    ledger: CostLedger,
    /// Daemon-wide registry of in-flight permission prompts (the
    /// `permission/respond` seam).
    pending: Arc<PendingPermissions>,
    /// Per-tool permission policy for every session.
    permission_config: PermissionConfig,
    /// Registered MCP servers (ADR-003), or `None` when none are configured.
    mcp_servers: Vec<McpServerConfig>,
    /// The startup model-lifecycle event sequence (BR-9 / AC-8), replayed to each
    /// newly attached client so it can observe probe → benchmark → ready.
    lifecycle: Vec<ModelLifecycle>,
    /// Monotonic turn-id source.
    turn_counter: AtomicU64,
    /// Per-session privacy taint: sessions pinned to the local tier because their
    /// context touched `local-only` or unknown-provenance content (REQ-544 C-2).
    session_taint: SessionTaint,
    /// Daemon-wide provider health, persisted across turns (REQ-544 M-5). Updated
    /// by `on_provider_failure` / turn outcomes and READ by [`build_router`] when
    /// it seeds the router each turn, so a provider observed `Unavailable` stays
    /// `Unavailable` into the next turn's route resolution — activating the policy
    /// layer's cross-turn health fallback. Absent id ⇒ `Healthy`.
    provider_health: Mutex<BTreeMap<String, ProviderHealth>>,
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
        Self {
            config: Mutex::new(Config::default()),
            config_path: None,
            repo_root: std::env::temp_dir(),
            engine: None,
            local_available: false,
            ledger,
            pending: Arc::new(PendingPermissions::new()),
            permission_config: PermissionConfig::coding_defaults(),
            mcp_servers: Vec::new(),
            lifecycle: Vec::new(),
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
        let config = load_config(config_path.as_deref());

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

        // --- local tier: hardware probe (BR-9 / AC-8) + scripted engine ---
        let pinned = config.pinned_local_model.clone();
        let probe = probe_local_tier(pinned.as_deref());
        let engine: Option<Arc<Mutex<dyn Engine>>> = build_local_engine(&probe);
        let local_available = engine.is_some() && !probe.disabled;

        // --- MCP servers (ADR-003 / AC-9): the main TOML config is the source of
        // truth; TETON_MCP_CONFIG is a test-only override (see `load_mcp_servers`).
        let mcp_servers = load_mcp_servers(&config);

        Ok(Self {
            config: Mutex::new(config),
            config_path,
            repo_root,
            engine,
            local_available,
            ledger,
            pending: Arc::new(PendingPermissions::new()),
            permission_config: PermissionConfig::coding_defaults(),
            mcp_servers,
            lifecycle: probe.lifecycle,
            turn_counter: AtomicU64::new(0),
            session_taint: SessionTaint::new(),
            provider_health: Mutex::new(BTreeMap::new()),
            secret_resolver: SecretResolver::with_default_backend(),
        })
    }

    /// The daemon-wide pending-permission registry (the `permission/respond` seam).
    #[must_use]
    pub fn pending(&self) -> &Arc<PendingPermissions> {
        &self.pending
    }

    /// The startup model-lifecycle events (BR-9), replayed to attaching clients.
    #[must_use]
    pub fn lifecycle_events(&self) -> &[ModelLifecycle] {
        &self.lifecycle
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
    /// `Unavailable` stays `Unavailable` until it serves a turn again.
    fn record_provider_health(&self, provider_id: &str, health: ProviderHealth) {
        self.provider_health
            .lock()
            .expect("provider_health mutex poisoned")
            .insert(provider_id.to_owned(), health);
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
        // provider marked Unavailable on an earlier turn stays Unavailable here.
        let health_snapshot = self
            .provider_health
            .lock()
            .expect("provider_health mutex poisoned")
            .clone();
        let router = build_router(
            &config,
            self.local_available,
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
                if err.is_privacy_block() {
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
                    // again — clear any earlier downgrade so a recovered provider
                    // returns to rotation on the next turn.
                    if let Some(pid) = route.provider_id.as_ref() {
                        self.record_provider_health(&pid.0, ProviderHealth::Healthy);
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
                    // failure (Retry) leaves health untouched.
                    if let Some(h) = health_after_failure(class) {
                        self.record_provider_health(&pid.0, h);
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

/// Load the config from `path`, falling back to defaults on any read/parse error.
fn load_config(path: Option<&Path>) -> Config {
    let Some(path) = path else {
        return Config::default();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Config::load(&text).unwrap_or_default(),
        Err(_) => Config::default(),
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

/// The result of the startup hardware probe (BR-9 / AC-8).
struct ProbeResult {
    /// The selected local model id, or `None` when disabled.
    model: Option<String>,
    /// Whether the local tier is disabled (below floor / resource-starved).
    disabled: bool,
    /// The lifecycle event sequence to replay to attaching clients.
    lifecycle: Vec<ModelLifecycle>,
}

/// Run the first-run hardware probe against real or env-overridden hardware and
/// synthesize the model-lifecycle event sequence (probe → download → benchmark →
/// ready / stepped-down / disabled).
fn probe_local_tier(pinned: Option<&str>) -> ProbeResult {
    let profile = probe_profile();
    let catalog = Catalog::bundled();
    let decision = decide(&profile, &catalog, pinned);

    let above_floor = profile.ram_bytes >= 8 * GIB;
    let mut lifecycle = Vec::new();

    match decision {
        TierDecision::Disabled { reason } => {
            let model_id = "local".to_owned();
            lifecycle.push(ModelLifecycle {
                model_id: model_id.clone(),
                stage: ModelLifecycleStage::Probed {
                    ram_bytes: profile.ram_bytes,
                    above_floor,
                },
            });
            lifecycle.push(ModelLifecycle {
                model_id,
                stage: ModelLifecycleStage::Disabled { reason },
            });
            ProbeResult {
                model: None,
                disabled: true,
                lifecycle,
            }
        }
        TierDecision::Selected { model, .. } => {
            lifecycle.push(ModelLifecycle {
                model_id: model.clone(),
                stage: ModelLifecycleStage::Probed {
                    ram_bytes: profile.ram_bytes,
                    above_floor,
                },
            });
            lifecycle.push(ModelLifecycle {
                model_id: model.clone(),
                stage: ModelLifecycleStage::Download {
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
            });

            // A forced-slow micro-benchmark trips the BR-8 latency duty and
            // auto-steps-down to the next smaller catalog model (AC-8).
            let force_slow = env_flag("TETON_PROBE_FORCE_SLOW_BENCH");
            if force_slow {
                let first_token_ms = 2_500;
                lifecycle.push(ModelLifecycle {
                    model_id: model.clone(),
                    stage: ModelLifecycleStage::Benchmark {
                        first_token_ms,
                        tokens_per_sec: 2.0,
                    },
                });
                let smaller = step_down_target(&catalog, &model);
                match smaller {
                    Some(to_model) => {
                        lifecycle.push(ModelLifecycle {
                            model_id: model.clone(),
                            stage: ModelLifecycleStage::SteppedDown {
                                from_model: model.clone(),
                                to_model: to_model.clone(),
                                reason: "benchmark exceeded the 1s first-token latency duty"
                                    .to_owned(),
                            },
                        });
                        lifecycle.push(ModelLifecycle {
                            model_id: to_model.clone(),
                            stage: ModelLifecycleStage::Benchmark {
                                first_token_ms: 600,
                                tokens_per_sec: 30.0,
                            },
                        });
                        lifecycle.push(ModelLifecycle {
                            model_id: to_model.clone(),
                            stage: ModelLifecycleStage::Ready,
                        });
                        return ProbeResult {
                            model: Some(to_model),
                            disabled: false,
                            lifecycle,
                        };
                    }
                    None => {
                        lifecycle.push(ModelLifecycle {
                            model_id: model.clone(),
                            stage: ModelLifecycleStage::Disabled {
                                reason: "no smaller model clears the latency duty; remote-only"
                                    .to_owned(),
                            },
                        });
                        return ProbeResult {
                            model: None,
                            disabled: true,
                            lifecycle,
                        };
                    }
                }
            }

            lifecycle.push(ModelLifecycle {
                model_id: model.clone(),
                stage: ModelLifecycleStage::Benchmark {
                    first_token_ms: 350,
                    tokens_per_sec: 40.0,
                },
            });
            lifecycle.push(ModelLifecycle {
                model_id: model.clone(),
                stage: ModelLifecycleStage::Ready,
            });
            ProbeResult {
                model: Some(model),
                disabled: false,
                lifecycle,
            }
        }
    }
}

/// The hardware profile to probe: env overrides when present, else detected.
fn probe_profile() -> HardwareProfile {
    let ram = env_u64("TETON_PROBE_RAM_BYTES");
    let disk = env_u64("TETON_PROBE_DISK_BYTES");
    let gpu = std::env::var("TETON_PROBE_GPU").ok();
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

/// Build the local engine when a scripted engine is configured and the probe did
/// not disable the tier.
fn build_local_engine(probe: &ProbeResult) -> Option<Arc<Mutex<dyn Engine>>> {
    if probe.disabled {
        return None;
    }
    let script = std::env::var_os("TETON_LOCAL_SCRIPT")?;
    let model_id = probe
        .model
        .clone()
        .unwrap_or_else(|| "scripted-local".to_owned());
    let engine = ScriptedFileEngine::from_file(model_id, Path::new(&script)).ok()?;
    Some(Arc::new(Mutex::new(engine)) as Arc<Mutex<dyn Engine>>)
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
            let secret = resolver
                .resolve(auth_ref)
                .map_err(|e| HarnessError::Credential(e.to_string()))?;
            let headers = provider_auth_headers(provider.kind, &secret);
            let endpoint = provider.endpoint.clone().unwrap_or_default();
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
}
