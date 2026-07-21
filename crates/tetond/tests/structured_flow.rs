//! Structured-mode acceptance harness (AC-3, BR-3, OQ-5, D-4).
//!
//! A demo requirement flows the full ADLC — spec → architect → implement →
//! review — through the real phase machine, artifact gates, router, and (for the
//! implement phase) the real remote turn loop + egress + cost ledger. It asserts:
//!
//! 1. **Per-phase routing is observable** in `route_decided`: a frontier model on
//!    spec/architect/review, the configured cheap model on implement (AC-3).
//! 2. **Gates carry artifacts across `phase_transition`** (D-4): each transition
//!    names the artifact(s) that unlocked it.
//! 3. **The implement turn actually executes remotely** and **carries the task
//!    artifact in context** — the cheap-model-viability mechanism: the task
//!    artifact's text reaches the provider request, and the remote model edits a
//!    real file through the loop.
//! 4. **A fresh repo (no prior `.teton/`) works** via bundled generic templates
//!    (OQ-5): the flow scaffolds, authors, and runs from nothing.
//! 5. **Freeform never requires artifacts** (BR-3).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use teton_core::entities::RoutingPolicy;
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;
use teton_core::ToolCallTier;

use teton_protocol::events::{Event, PhaseTransition};
use teton_protocol::{Phase, ProviderId, SessionId};

use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{CapabilityProfile, OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::{Egress, NoopSink};
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, NoopProvenanceHook,
    PendingPermissions, PermissionConfig, PermissionGate, RemoteProviderSource, SessionEvents,
    ToolContext, ToolRegistry,
};
use tetond::router::Router;
use tetond::structured::{ArtifactKind, ArtifactStore, PhaseMachine};

/// A distinctive marker planted in the task artifact; it must reach the provider
/// request when the implement turn runs (proving the artifact is in context).
const TASK_MARKER: &str = "IMPLEMENT-EXACTLY-THIS-VALUE-42";

// --------------------------------------------------------------------------
// A scripted OpenAI-compatible transport that also records request bodies.
// --------------------------------------------------------------------------

#[derive(Clone, Default)]
struct RecordingTransport {
    bodies: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl RecordingTransport {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.requests
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&request.body).into_owned());
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
            status: 200,
            body: Box::pin(futures::stream::once(async move { Ok(body.into_bytes()) })),
        })
    }
}

fn sse_turn(content: &str, tool: Option<(&str, &str, &str)>, input: u64, output: u64) -> String {
    let mut s = String::new();
    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": content } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": id, "function": { "name": name, "arguments": args }
            }]}}]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let fin =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {fin}\n\n"));
    } else {
        let fin = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {fin}\n\n"));
    }
    let usage =
        serde_json::json!({ "usage": { "prompt_tokens": input, "completion_tokens": output } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

fn temp_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-structured-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/demo.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
    root
}

fn native() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Native,
        parallel_calls: true,
        max_context: 200_000,
    }
}

fn policy(phase: CorePhase, provider: &str, fallback: Option<&str>) -> RoutingPolicy {
    RoutingPolicy {
        phase,
        provider_id: provider.to_owned(),
        fallback_id: fallback.map(str::to_owned),
    }
}

/// The AC-3 routing shape: frontier on spec/architect/review, cheap on implement.
fn structured_router() -> Router {
    Router::new(
        vec![
            policy(CorePhase::Spec, "anthropic", Some("deepseek")),
            policy(CorePhase::Architect, "anthropic", Some("deepseek")),
            policy(CorePhase::Implement, "deepseek", Some("anthropic")),
            policy(CorePhase::Review, "anthropic", Some("deepseek")),
        ],
        "deepseek",
        "local",
    )
    .with_provider(
        "anthropic",
        "claude-opus-4",
        native(),
        ProviderHealth::Healthy,
    )
    .with_provider(
        "deepseek",
        "deepseek-chat",
        native(),
        ProviderHealth::Healthy,
    )
}

/// Map a protocol phase back to the routing axis the router evaluates.
fn core_of(phase: Phase) -> CorePhase {
    match phase {
        Phase::Spec => CorePhase::Spec,
        Phase::Architect => CorePhase::Architect,
        Phase::Implement => CorePhase::Implement,
        Phase::Review => CorePhase::Review,
        Phase::Io => CorePhase::Io,
        Phase::Freeform => CorePhase::Freeform,
    }
}

/// Author every scaffolded artifact so the gates pass; the task artifact plants
/// the distinctive marker and the concrete edit for the implement turn.
fn author_artifacts(store: &ArtifactStore) {
    store
        .write(
            "demo",
            ArtifactKind::Requirement,
            "# Requirement demo: bump VALUE\n\nChange VALUE in src/demo.rs from 1 to 2.",
        )
        .unwrap();
    store
        .write(
            "demo",
            ArtifactKind::Plan,
            "# Plan\n\nEdit the single constant; verify by re-reading.",
        )
        .unwrap();
    store
        .write(
            "demo",
            ArtifactKind::Task,
            &format!(
                "# Task: {TASK_MARKER}\n\nIn src/demo.rs change `pub const VALUE: u32 = 1;` to \
                 `pub const VALUE: u32 = 2;`."
            ),
        )
        .unwrap();
}

async fn collect_events(
    sub: &mut tetond::broadcast::Subscription,
) -> Vec<teton_protocol::events::EventEnvelope> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        out.push(env);
    }
    out
}

// --------------------------------------------------------------------------
// The full flow.
// --------------------------------------------------------------------------

#[tokio::test]
async fn demo_requirement_flows_all_four_phases_with_per_phase_routing_and_real_implement() {
    let repo = temp_repo();
    let store = ArtifactStore::new(&repo);

    // OQ-5: a repo with no prior `.teton/` scaffolds from the bundled generic
    // templates, then the spec/architect turns author them.
    assert!(!store.teton_dir().exists());
    store.scaffold("demo", "bump VALUE").expect("scaffold");
    author_artifacts(&store);

    let router = structured_router();
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(256);
    let session = SessionId::from("sess-structured");

    // The remote implement turn edits the file, then finishes (deepseek is a
    // Native tool-caller in the fixture, so no verification is forced).
    let transport = RecordingTransport::with_bodies(vec![
        sse_turn(
            "Applying the task edit.",
            Some((
                "call_1",
                "edit",
                r#"{"path":"src/demo.rs","old_string":"pub const VALUE: u32 = 1;","new_string":"pub const VALUE: u32 = 2;"}"#,
            )),
            300,
            40,
        ),
        sse_turn("Done — VALUE is now 2.", None, 320, 12),
    ]);
    let requests = Arc::clone(&transport.requests);
    let ledger = Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink)).expect("ledger"),
    );
    let egress =
        Egress::new(transport, Vec::new(), Arc::new(NoopSink)).with_cost_meter(ledger.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));

    let mut machine = PhaseMachine::structured("demo");
    let mut routed: Vec<(Phase, String)> = Vec::new();
    let mut transitions: Vec<PhaseTransition> = Vec::new();

    for phase in [
        Phase::Spec,
        Phase::Architect,
        Phase::Implement,
        Phase::Review,
    ] {
        assert_eq!(machine.phase(), phase, "machine tracks the flow position");

        // (1) Route this phase and broadcast the decision (BR-5, AC-3).
        let route = router.resolve_structured(core_of(phase));
        let provider_id = route.provider_id.as_ref().unwrap().0.clone();
        routed.push((phase, provider_id.clone()));
        router.emit_route_decided(&bus, Some(session.clone()), &route);

        // (3) The implement phase actually runs, remotely, carrying the task
        // artifact in context.
        if phase == Phase::Implement {
            let config = route.harness.clone();
            let tools = ToolRegistry::with_builtins();
            let tool_ctx = ToolContext::new(&repo);
            let system = build_system_prompt(&tools, &config);
            let mut ctx = ContextManager::new(system, config.context_budget_tokens);
            for artifact in machine.context_artifacts(&store) {
                assert_eq!(
                    artifact.kind,
                    ArtifactKind::Task,
                    "implement carries the task"
                );
                ctx.push_user(format!(
                    "Task artifact ({}):\n{}",
                    artifact.path, artifact.content
                ));
            }
            ctx.push_user("Execute the task above, then finish.");

            let pending = Arc::new(PendingPermissions::new());
            let gate = PermissionGate::new(
                session.clone(),
                PermissionConfig::permissive(),
                Arc::clone(&bus),
                Arc::clone(&pending),
            );
            let events = SessionEvents::new(Arc::clone(&bus), session.clone());
            let mut hook = NoopProvenanceHook;
            let mut source = RemoteProviderSource::new(
                &provider,
                &egress,
                "deepseek",
                route.model.clone().unwrap(),
                session.clone(),
            )
            .with_phase(Phase::Implement);

            let outcome = run_session_turn_with_source(
                &mut source,
                &tools,
                &tool_ctx,
                &gate,
                &events,
                &mut ctx,
                &config,
                &mut hook,
                None,
            )
            .await
            .expect("the implement turn runs remotely");
            assert!(outcome.edited, "the remote implement turn edited the file");
        }

        // (2) Advance the gate (except past the terminal review phase).
        if phase != Phase::Review {
            let transition = machine.try_advance(&store).expect("gate passes");
            bus.publish(
                Some(session.clone()),
                Event::PhaseTransition(transition.clone()),
            );
            transitions.push(transition);
        }
    }

    // --- The implement turn carried the task artifact into the provider request ---
    // (Scoped so the guard is not held across the later `collect_events` await.)
    {
        let sent = requests.lock().unwrap();
        assert!(!sent.is_empty(), "the implement turn made remote calls");
        assert!(
            sent[0].contains(TASK_MARKER),
            "the task artifact must reach the model (cheap-model-viability); request: {}",
            sent[0]
        );

        // REQ-544 M-8: the remote request is a real system prompt plus role-typed
        // messages, NOT one collapsed user blob. The first request already carries
        // a system message + a user message; the second (after the tool call)
        // additionally carries an assistant turn — proof the roles are preserved.
        assert!(
            sent[0].contains(r#""role":"system""#),
            "the request must carry a system message, not system:None; request: {}",
            sent[0]
        );
        assert!(
            sent[0].contains(r#""role":"user""#),
            "the request must carry a user message; request: {}",
            sent[0]
        );
        let last = sent.last().unwrap();
        assert!(
            last.contains(r#""role":"assistant""#),
            "a follow-up request must preserve the prior assistant turn as its own \
             role-typed message (not folded into a user blob); request: {last}"
        );
    }

    // --- The file was really edited by the remote implement turn ---
    let updated = std::fs::read_to_string(repo.join("src/demo.rs")).unwrap();
    assert!(
        updated.contains("pub const VALUE: u32 = 2;"),
        "implement phase did not edit the file: {updated}"
    );

    // --- Cost was attributed to the implement phase and the cheap provider (BR-2) ---
    let rows = ledger.all_records().expect("read ledger");
    assert!(!rows.is_empty(), "the implement turn billed cost");
    for row in &rows {
        assert_eq!(row.phase, Some(Phase::Implement));
        assert_eq!(row.provider_id, "deepseek");
        assert_eq!(row.model, "deepseek-chat");
    }

    // --- (1) Per-phase routing is observable: frontier on spec/architect/review,
    //         cheap on implement (AC-3). ---
    assert_eq!(
        routed,
        vec![
            (Phase::Spec, "anthropic".to_owned()),
            (Phase::Architect, "anthropic".to_owned()),
            (Phase::Implement, "deepseek".to_owned()),
            (Phase::Review, "anthropic".to_owned()),
        ]
    );

    // --- (2) The gates produced three phase_transitions carrying artifacts ---
    assert_eq!(transitions.len(), 3);
    assert_eq!(transitions[0].from_phase, Some(Phase::Spec));
    assert_eq!(transitions[0].to_phase, Phase::Architect);
    assert_eq!(
        transitions[0].artifacts[0].path,
        ".teton/demo/requirement.md"
    );
    assert_eq!(transitions[1].to_phase, Phase::Implement);
    assert_eq!(
        transitions[1].artifacts.len(),
        2,
        "architect carries plan + task"
    );
    assert_eq!(transitions[2].to_phase, Phase::Review);
    assert!(machine.is_terminal(), "review is terminal");

    // --- Events on the wire: four route_decided (AC-3) and three phase_transition ---
    let evs = collect_events(&mut sub).await;
    let route_decided: Vec<_> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::RouteDecided(rd) => Some(rd),
            _ => None,
        })
        .collect();
    assert_eq!(route_decided.len(), 4, "one route_decided per phase (AC-3)");
    // The route_decided phases and providers match the AC-3 shape, on the wire.
    let implement_rd = route_decided
        .iter()
        .find(|rd| rd.phase == Some(Phase::Implement))
        .expect("implement route_decided");
    assert_eq!(implement_rd.provider_id, ProviderId::from("deepseek"));
    assert_eq!(implement_rd.model.as_deref(), Some("deepseek-chat"));
    for spec_phase in [Phase::Spec, Phase::Architect, Phase::Review] {
        let rd = route_decided
            .iter()
            .find(|rd| rd.phase == Some(spec_phase))
            .expect("frontier route_decided");
        assert_eq!(rd.provider_id, ProviderId::from("anthropic"));
    }
    let pt_count = evs
        .iter()
        .filter(|e| matches!(&e.event, Event::PhaseTransition(_)))
        .count();
    assert_eq!(pt_count, 3, "three phase_transition events (D-4)");

    std::fs::remove_dir_all(&repo).ok();
}

// --------------------------------------------------------------------------
// BR-3: freeform is the default and never requires structured artifacts.
// --------------------------------------------------------------------------

#[tokio::test]
async fn freeform_mode_requires_no_artifacts() {
    let repo = temp_repo();
    let store = ArtifactStore::new(&repo);
    let machine = PhaseMachine::freeform();

    assert_eq!(machine.phase(), Phase::Freeform);
    // A freeform session has no `.teton/` and needs none — no artifacts, no gates.
    assert!(machine.context_artifacts(&store).is_empty());
    assert!(!store.teton_dir().exists());
    assert!(machine.is_terminal(), "freeform is a single-phase flow");

    std::fs::remove_dir_all(&repo).ok();
}
