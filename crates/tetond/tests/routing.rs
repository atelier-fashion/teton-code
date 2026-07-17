//! Router acceptance harness (BR-5, BR-6, BR-8, AC-3, AC-7, and the BR-1/BR-2
//! by-construction proof).
//!
//! The router is the wiring layer over the pure policy evaluator: it turns a
//! phase (or a freeform prompt) into a provider + a legible reason, applies the
//! BR-6 degradation profile, and — for remote calls — builds the egress-choke
//! context that makes privacy (BR-1) and cost recording (BR-2) hold by
//! construction. These tests drive the router the way the daemon will and assert:
//!
//! 1. Structured-mode calls route per the policy table and each emits a
//!    `route_decided` whose reason names the rule that fired (BR-5, AC-3 backend).
//! 2. Freeform heuristic decisions also emit `route_decided` with reasons (BR-5).
//! 3. A simulated mid-session provider failure falls back per its failure class,
//!    emits `provider_degraded`, and the session completes on the fallback (AC-7).
//! 4. When the local tier is unavailable the router bypasses it to a remote
//!    provider rather than blocking the loop (BR-8).
//! 5. A weak-capability provider is routed under the reduced harness profile
//!    (smaller tool set, shorter loop, mandatory verification) (BR-6).
//! 6. A routed *remote* call produces a `CostRecord` **and** is subject to
//!    boundary inspection at the same choke point — the BR-1/BR-2 proof.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use teton_core::entities::{BoundaryMode, PrivacyBoundary, RoutingPolicy};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;
use teton_core::ToolCallTier;

use teton_protocol::events::Event;
use teton_protocol::{Phase as ProtoPhase, ProviderId, SessionId};

use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};
use teton_providers::{CapabilityProfile, FailureClass};

use tetond::broadcast::{EventBus, Subscription};
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::{Egress, EgressError, NoopSink, Provenance};
use tetond::router::Router;

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

fn native() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Native,
        parallel_calls: true,
        max_context: 200_000,
    }
}

fn degraded() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Degraded,
        parallel_calls: false,
        max_context: 32_000,
    }
}

fn policy(phase: CorePhase, provider: &str, fallback: Option<&str>) -> RoutingPolicy {
    RoutingPolicy {
        phase,
        provider_id: provider.to_owned(),
        fallback_id: fallback.map(str::to_owned),
    }
}

/// A router with the AC-3 routing shape: frontier on spec/architect/review, the
/// cheap provider on implement, the local tier on io — plus a freeform default
/// (deepseek) and local tier, all healthy.
fn structured_router() -> Router {
    Router::new(
        vec![
            policy(CorePhase::Spec, "anthropic", Some("deepseek")),
            policy(CorePhase::Architect, "anthropic", Some("deepseek")),
            policy(CorePhase::Implement, "deepseek", Some("anthropic")),
            policy(CorePhase::Review, "anthropic", Some("deepseek")),
            policy(CorePhase::Io, "local", None),
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
    .with_provider(
        "local",
        "qwen2.5-coder-3b",
        native(),
        ProviderHealth::Healthy,
    )
}

/// A `Transport` that returns a canned Anthropic-shaped SSE body carrying a
/// scripted `(input_tokens, output_tokens)` per call — the network stand-in used
/// by the egress-backed tests (mirrors `tests/cost_attribution.rs`).
#[derive(Clone, Default)]
struct ScriptedTransport {
    usages: Arc<Mutex<VecDeque<(u64, u64)>>>,
}

impl ScriptedTransport {
    fn with_script(script: &[(u64, u64)]) -> Self {
        Self {
            usages: Arc::new(Mutex::new(script.iter().copied().collect())),
        }
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let (input, output) = self.usages.lock().unwrap().pop_front().unwrap_or((0, 0));
        Ok(TransportResponse {
            status: 200,
            body: anthropic_body(input, output),
        })
    }
}

fn anthropic_body(input: u64, output: u64) -> ByteStream {
    let s = format!(
        "event: message_start\n\
         data: {{\"message\":{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":1}}}}}}\n\n\
         event: message_delta\n\
         data: {{\"usage\":{{\"output_tokens\":{output}}}}}\n\n\
         event: message_stop\ndata: {{}}\n\n"
    );
    Box::pin(futures::stream::once(async move { Ok(s.into_bytes()) }))
}

fn request(body: &str) -> TransportRequest {
    TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

async fn drain(mut body: ByteStream) {
    while let Some(chunk) = body.next().await {
        chunk.expect("scripted chunk is ok");
    }
}

/// The real [`Egress`] choke point over a scripted transport, metered by an
/// in-memory cost ledger — the same wiring the daemon uses in production.
fn egress_with_ledger(
    transport: ScriptedTransport,
    boundaries: Vec<PrivacyBoundary>,
) -> (Arc<CostLedger>, Egress<ScriptedTransport>) {
    let ledger = Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger"),
    );
    let egress =
        Egress::new(transport, boundaries, Arc::new(NoopSink)).with_cost_meter(ledger.clone());
    (ledger, egress)
}

/// Drain every event currently buffered on `sub` (short timeout marks the end).
async fn collect_events(sub: &mut Subscription) -> Vec<teton_protocol::events::EventEnvelope> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        out.push(env);
    }
    out
}

// --------------------------------------------------------------------------
// 1. Structured-mode policy routing + legible route_decided (BR-5, AC-3)
// --------------------------------------------------------------------------

#[tokio::test]
async fn structured_mode_routes_per_policy_and_route_decided_names_the_rule() {
    let router = structured_router();
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-structured");

    // AC-3 shape: frontier on spec/architect/review, cheap on implement, local io.
    let expected = [
        (CorePhase::Spec, "anthropic", ProtoPhase::Spec),
        (CorePhase::Architect, "anthropic", ProtoPhase::Architect),
        (CorePhase::Implement, "deepseek", ProtoPhase::Implement),
        (CorePhase::Review, "anthropic", ProtoPhase::Review),
        (CorePhase::Io, "local", ProtoPhase::Io),
    ];

    for (phase, provider, _) in expected {
        let route = router.resolve_structured(phase);
        assert_eq!(
            route.provider_id.as_ref().unwrap().0,
            provider,
            "phase {phase} routes to {provider} per policy"
        );
        // BR-5: the reason names the rule that fired (the routing policy).
        assert!(
            route.reason.contains("routing policy"),
            "reason names the rule: {}",
            route.reason
        );
        router.emit_route_decided(&bus, Some(session.clone()), &route);
    }

    let events = collect_events(&mut sub).await;
    let decided: Vec<_> = events
        .iter()
        .filter(|e| e.event_name() == "route_decided")
        .collect();
    assert_eq!(
        decided.len(),
        expected.len(),
        "one route_decided per structured decision (BR-5)"
    );
    for (env, (_, provider, proto)) in decided.iter().zip(expected.iter()) {
        match &env.event {
            Event::RouteDecided(rd) => {
                assert_eq!(rd.provider_id, ProviderId::from(*provider));
                assert_eq!(rd.phase, Some(*proto));
                assert!(!rd.reason.is_empty(), "route_decided carries a reason");
            }
            other => panic!("expected route_decided, got {other:?}"),
        }
        assert_eq!(env.session_id.as_ref(), Some(&session));
    }
}

// --------------------------------------------------------------------------
// 2. Freeform heuristic decisions also emit route_decided (BR-5)
// --------------------------------------------------------------------------

#[tokio::test]
async fn freeform_heuristic_decisions_emit_route_decided_with_reasons() {
    let router = structured_router();
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-freeform");

    // An auxiliary duty → local tier; a coding turn → configured default.
    let aux = router.resolve_freeform("summarize this diff for the changelog");
    assert_eq!(aux.provider_id.as_ref().unwrap().0, "local");
    assert!(aux.phase.is_none(), "freeform carries no phase");
    router.emit_route_decided(&bus, Some(session.clone()), &aux);

    let coding = router.resolve_freeform("implement the retry backoff");
    assert_eq!(coding.provider_id.as_ref().unwrap().0, "deepseek");
    router.emit_route_decided(&bus, Some(session.clone()), &coding);

    let events = collect_events(&mut sub).await;
    let decided: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::RouteDecided(rd) => Some(rd),
            _ => None,
        })
        .collect();
    assert_eq!(
        decided.len(),
        2,
        "every freeform decision still emits route_decided (BR-5)"
    );
    for rd in &decided {
        assert!(rd.phase.is_none(), "freeform route_decided has no phase");
        assert!(!rd.reason.is_empty(), "heuristic decision carries a reason");
    }
    assert!(
        decided[0].reason.to_lowercase().contains("local"),
        "auxiliary reason names the local tier: {}",
        decided[0].reason
    );
    assert!(
        decided[1].reason.contains("default"),
        "coding reason names the default: {}",
        decided[1].reason
    );
}

// --------------------------------------------------------------------------
// 3. Fallback on simulated provider failure completes the session (AC-7)
// --------------------------------------------------------------------------

#[tokio::test]
async fn simulated_provider_failure_falls_back_and_completes_emitting_provider_degraded() {
    // Implement primary = a flaky provider whose fallback is anthropic.
    let router = Router::new(
        vec![policy(CorePhase::Implement, "flaky", Some("anthropic"))],
        "anthropic",
        "local",
    )
    .with_provider("flaky", "flaky-model", native(), ProviderHealth::Healthy)
    .with_provider(
        "anthropic",
        "claude-opus-4",
        native(),
        ProviderHealth::Healthy,
    );

    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-ac7");

    // The primary is selected, then fails mid-turn with a fallback-class error.
    let primary = router.resolve_structured(CorePhase::Implement);
    assert_eq!(primary.provider_id.as_ref().unwrap().0, "flaky");

    let outcome = router.on_provider_failure(
        Some(CorePhase::Implement),
        "flaky",
        FailureClass::MalformedResponse,
    );
    let degraded = outcome
        .degraded
        .clone()
        .expect("a fallback-class failure surfaces provider_degraded");
    router.emit_provider_degraded(&bus, Some(session.clone()), degraded);

    let fallback_route = outcome
        .route
        .expect("the session continues on the fallback");
    assert_eq!(fallback_route.provider_id.as_ref().unwrap().0, "anthropic");

    // The session COMPLETES via the fallback: a routed remote call on the fallback
    // provider goes through egress and produces a CostRecord (BR-2).
    let (ledger, egress) =
        egress_with_ledger(ScriptedTransport::with_script(&[(900, 300)]), Vec::new());
    let ctx = router
        .egress_context(&fallback_route, session.clone())
        .expect("remote egress context");
    let resp = egress
        .send(request("implement body"), &Provenance::empty(), &ctx)
        .await
        .expect("the fallback call is allowed");
    drain(resp.body).await;

    let rows = ledger.all_records().expect("read rows");
    assert_eq!(rows.len(), 1, "the fallback call completed and was billed");
    assert_eq!(rows[0].provider_id, "anthropic");
    assert_eq!(rows[0].phase, Some(ProtoPhase::Implement));

    // provider_degraded was broadcast, naming the failed provider and the fallback.
    let events = collect_events(&mut sub).await;
    let pd: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ProviderDegraded(pd) => Some(pd),
            _ => None,
        })
        .collect();
    assert_eq!(pd.len(), 1, "exactly one provider_degraded (AC-7)");
    assert_eq!(pd[0].provider_id, ProviderId::from("flaky"));
    assert_eq!(
        pd[0].fallback_id.as_ref().expect("fallback named"),
        &ProviderId::from("anthropic")
    );
}

#[tokio::test]
async fn a_malformed_tool_call_degrades_in_place_rather_than_failing() {
    // The other side of "falls back per failure class": a weak-tool-calling
    // failure keeps the provider but forces the reduced BR-6 profile, still
    // completing rather than aborting.
    let router = structured_router();
    let outcome = router.on_provider_failure(
        Some(CorePhase::Implement),
        "deepseek",
        FailureClass::MalformedToolCall,
    );
    let degraded = outcome
        .degraded
        .expect("degrade surfaces provider_degraded");
    assert!(
        degraded.fallback_id.is_none(),
        "an in-place degrade names no fallback"
    );
    let route = outcome.route.expect("continues on the same provider");
    assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
    assert!(route.harness.require_verification, "reduced profile (BR-6)");
    assert_eq!(route.harness.max_tools, Some(5));
}

// --------------------------------------------------------------------------
// 4. Local tier unavailable → router bypasses without blocking (BR-8)
// --------------------------------------------------------------------------

#[tokio::test]
async fn local_tier_unavailable_bypasses_without_blocking() {
    let router = structured_router().with_local_available(false);

    // An auxiliary duty would normally go local; with the local tier unavailable
    // the router must bypass it — selecting the default — rather than blocking.
    let route = router.resolve_freeform("summarize the failing test output");
    assert!(
        route.selected(),
        "BR-8: the router must not block on the local tier"
    );
    assert_eq!(
        route.provider_id.as_ref().unwrap().0,
        "deepseek",
        "bypassed to the configured default"
    );
    assert!(
        route.reason.contains("unavailable") && route.reason.contains("bypass"),
        "reason explains the BR-8 bypass: {}",
        route.reason
    );
    // The per-turn harness input is still produced — the loop can proceed.
    assert!(route.turn_route().is_some());
}

// --------------------------------------------------------------------------
// 5. Weak-capability provider gets the degraded harness profile (BR-6)
// --------------------------------------------------------------------------

#[tokio::test]
async fn weak_capability_provider_gets_degraded_harness_profile() {
    // Implement routes to a weak-tool-calling provider.
    let router = Router::new(
        vec![policy(CorePhase::Implement, "kimi", None)],
        "kimi",
        "local",
    )
    .with_provider("kimi", "kimi-k2", degraded(), ProviderHealth::Degraded);

    let route = router.resolve_structured(CorePhase::Implement);
    assert_eq!(route.provider_id.as_ref().unwrap().0, "kimi");

    // BR-6: reduced tool set, shorter loop, mandatory verification.
    assert!(route.harness.require_verification);
    assert_eq!(route.harness.max_tools, Some(5));
    assert!(route.harness.max_turns <= 5);
    // The degraded primary is kept (not failed over); the policy reason says so.
    assert!(
        route.reason.contains("reduced profile"),
        "reason: {}",
        route.reason
    );

    // The per-turn harness input the loop consumes carries that reduced profile.
    let turn = route.turn_route().expect("provider selected");
    assert!(turn.config.require_verification);
    assert_eq!(turn.model.as_deref(), Some("kimi-k2"));
}

// --------------------------------------------------------------------------
// 6. A routed remote call: CostRecord AND boundary inspection (BR-1/BR-2)
// --------------------------------------------------------------------------

#[tokio::test]
async fn routed_remote_call_produces_cost_record_and_passes_boundary_inspection() {
    let router = structured_router();
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-integration");

    // The spec phase routes to a remote (anthropic) provider, per policy.
    let route = router.resolve_structured(CorePhase::Spec);
    assert_eq!(route.provider_id.as_ref().unwrap().0, "anthropic");
    router.emit_route_decided(&bus, Some(session.clone()), &route);

    // Egress over a scripted transport, with a local-only boundary and the cost
    // ledger as meter — the SAME choke point the daemon uses in production.
    let (ledger, egress) = egress_with_ledger(
        ScriptedTransport::with_script(&[(1500, 600)]),
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }],
    );

    // (BR-2) A clean routed remote call produces exactly one attributed CostRecord.
    let ctx = router
        .egress_context(&route, session.clone())
        .expect("remote egress context");
    let resp = egress
        .send(request("public spec prompt"), &Provenance::empty(), &ctx)
        .await
        .expect("the clean routed call is allowed");
    drain(resp.body).await;

    // (BR-1) A routed call whose context intersects a local-only boundary is
    // blocked at the same choke point — proven by capture, not code inspection —
    // and is never billed.
    let blocked_ctx = router
        .egress_context(&route, session.clone())
        .expect("remote egress context");
    let err = egress
        .send(
            request("API_KEY=sk-live-DO-NOT-LEAK"),
            &Provenance::tainted_by("secrets/prod.env"),
            &blocked_ctx,
        )
        .await
        .expect_err("boundary content must be blocked on the routed path");
    assert!(matches!(err, EgressError::PrivacyBlocked { .. }));

    // Exactly one CostRecord: the clean call billed once (BR-2), the blocked call
    // billed zero (BR-1). Both hold by construction because the routed remote call
    // flows through egress.
    let rows = ledger.all_records().expect("read rows");
    assert_eq!(
        rows.len(),
        1,
        "one billed call; the blocked call is never billed"
    );
    assert_eq!(rows[0].provider_id, "anthropic");
    assert_eq!(rows[0].model, "claude-opus-4");
    assert_eq!(rows[0].phase, Some(ProtoPhase::Spec));
    assert_eq!(rows[0].session_id, "sess-integration");

    // The route_decided event fired for the routed call (BR-5).
    let events = collect_events(&mut sub).await;
    assert!(
        events.iter().any(|e| e.event_name() == "route_decided"),
        "the routed call emitted route_decided"
    );
}
