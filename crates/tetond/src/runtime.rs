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
//! - `TETON_CONFIG` — the TOML config file (providers, routing, boundaries).
//! - `TETON_MCP_CONFIG` — a JSON file of MCP server configs (ADR-003).
//! - `TETON_REPO_ROOT` — the repo the tools are jailed to.
//! - `TETON_PROBE_RAM_BYTES` / `TETON_PROBE_DISK_BYTES` / `TETON_PROBE_GPU` /
//!   `TETON_PROBE_FORCE_SLOW_BENCH` — hardware-probe overrides (BR-9 / AC-8).
//!
//! None of these are required in production (a real build resolves hardware and
//! weights itself); they exist so the whole daemon can be exercised end to end
//! against mocks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use teton_core::config::Config;
use teton_core::entities::{
    BoundaryMode, ModelProvider, PrivacyBoundary, ProviderCapabilities, ProviderKind, RoutingPolicy,
};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;

use teton_inference::catalog::Catalog;
use teton_inference::probe::{decide, GpuClass, HardwareProfile, TierDecision, GIB};
use teton_inference::{Completion, Engine, EngineError, GenParams};

use teton_protocol::events::{ModelLifecycle, ModelLifecycleStage};
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
    AnthropicAdapter, CapabilityProfile, OpenAiCompatAdapter, OpenAiCompatConfig, Provider,
};

use crate::broadcast::EventBus;
use crate::cost::{CostLedger, CostReport, GroupTotals, PriceTable};
use crate::egress::{Egress, HttpTransport};
use crate::harness::completion::RemoteProviderSource;
use crate::harness::context::NoopProvenanceHook;
use crate::harness::turn_loop::{run_session_turn_with_source, HarnessError};
use crate::harness::{
    build_system_prompt, ContextManager, LocalEngineSource, PendingPermissions, PermissionConfig,
    PermissionGate, SessionEvents, ToolContext, ToolRegistry,
};
use crate::mcp::{McpRegistry, McpServerConfig};
use crate::router::Router;

/// Separator between reply blocks in a `TETON_LOCAL_SCRIPT` file.
const SCRIPT_SEPARATOR: &str = "---";

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

        // --- MCP servers (ADR-003) ---
        let mcp_servers = load_mcp_servers();

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
        let router = build_router(&config, self.local_available, self.ledger.prices());

        // Resolve the initial route (BR-5): structured -> phase policy; freeform
        // -> heuristic. Emitting `route_decided` is the legibility promise.
        let core_phase = phase.map(to_core_phase);
        let mut route = match mode {
            SessionMode::Structured => {
                let ph = core_phase.unwrap_or(CorePhase::Implement);
                router.resolve_structured(ph)
            }
            SessionMode::Freeform => router.resolve_freeform(&prompt),
        };

        // Assemble the harness context, tools, and the permission gate once; a
        // fallback re-runs the loop against the same accumulated context.
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

            match result {
                Ok(outcome) => {
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

        let transport = HttpTransport::new()
            .map_err(|e| HarnessError::Engine(EngineError::Backend(e.to_string())))?;
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

/// Load MCP server configs from `TETON_MCP_CONFIG` (a JSON array), if set.
fn load_mcp_servers() -> Vec<McpServerConfig> {
    let Some(path) = std::env::var_os("TETON_MCP_CONFIG") else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
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
fn build_router(config: &Config, local_available: bool, prices: &PriceTable) -> Router {
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
        router = router.with_provider(
            p.id.clone(),
            billing_model(prices, &p.id),
            CapabilityProfile::from_core(p.capabilities),
            ProviderHealth::Healthy,
        );
    }
    router
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
        ProviderKind::OpenAiCompatible => ProtoProviderKind::OpenaiCompatible,
        ProviderKind::Anthropic => ProtoProviderKind::Anthropic,
        ProviderKind::Custom => ProtoProviderKind::Custom,
    }
}

fn to_core_kind(kind: ProtoProviderKind) -> ProviderKind {
    match kind {
        ProtoProviderKind::Local => ProviderKind::Local,
        ProtoProviderKind::OpenaiCompatible => ProviderKind::OpenAiCompatible,
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
}
