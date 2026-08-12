//! Router acceptance harness (BR-5, BR-6, BR-8, AC-3, AC-7, and the BR-1/BR-2
//! by-construction proof).
//!
//! The router is the wiring layer over the pure category resolver: it turns a
//! **category** into a provider + a legible reason, applies the BR-6 degradation
//! profile, and — for remote calls — builds the egress-choke context that makes
//! privacy (BR-1) and cost recording (BR-2) hold by construction. These tests
//! drive the router the way the daemon does and assert:
//!
//! 1. Structured-mode calls route per the configured table and each emits a
//!    `route_decided` whose reason names the binding that fired (BR-5, AC-3
//!    backend).
//! 2. **Freeform calls read the same table** (REQ-558 BR-1) and also emit
//!    `route_decided` with reasons (BR-5).
//! 3. A simulated mid-session provider failure falls back per its failure class,
//!    emits `provider_degraded`, and the session completes on the fallback (AC-7).
//! 4. When the local tier is unavailable a category bound to it fails over to its
//!    configured fallback rather than blocking the loop (BR-8).
//! 5. A weak-capability provider is routed under the reduced harness profile
//!    (smaller tool set, shorter loop, mandatory verification) (BR-6).
//! 6. A routed *remote* call produces a `CostRecord` **and** is subject to
//!    boundary inspection at the same choke point — the BR-1/BR-2 proof.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use teton_core::entities::ProviderKind;

use async_trait::async_trait;
use futures::StreamExt;

use teton_core::category::{
    category_for_phase, Category, CategoryTable, JudgmentCategory, Tier, TierBinding,
};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;
use teton_core::ToolCallTier;

use teton_inference::{ChatFormat, Engine, MockEngine};

use teton_protocol::events::Event;
use teton_protocol::{Phase as ProtoPhase, ProviderId, SessionId};

use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};
use teton_providers::{CapabilityProfile, FailureClass};

use tetond::broadcast::{EventBus, Subscription};
use tetond::classify::{self, ClassificationSignal};
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::{Egress, EgressError, NoopSink, Provenance};
use tetond::router::{to_protocol_phase, Route, Router};

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

fn native() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Native,
        parallel_calls: true,
        max_context: 200_000,
        ..CapabilityProfile::default()
    }
}

fn degraded() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Degraded,
        parallel_calls: false,
        max_context: 32_000,
        ..CapabilityProfile::default()
    }
}

fn tier(tier: Tier, provider: &str, fallback: Option<&str>) -> TierBinding {
    TierBinding {
        tier,
        provider_id: provider.to_owned(),
        fallback_id: fallback.map(str::to_owned),
    }
}

/// How the daemon routes a **structured** turn (`runtime.rs`): map the phase to
/// a category (ADR-C, no model call), resolve the category, then stamp the phase
/// back on for cost attribution only (BR-11, AC-9).
///
/// Written out here rather than hidden behind a router method because the two
/// halves are the point: the router never sees the phase, and the ledger still
/// does.
fn structured_route(router: &Router, phase: CorePhase) -> Route {
    let mut route = router.resolve(category_for_phase(phase));
    route.phase = Some(to_protocol_phase(phase));
    route
}

/// A router with the AC-3 routing shape translated into tiers: frontier on
/// `think` (design/debug/review), the cheap provider on `build` (edit/shell),
/// the local tier on `scan` (digest/triage) with a remote fallback — plus a
/// declared default (deepseek) and a local tier, all healthy.
fn structured_router() -> Router {
    Router::new(
        CategoryTable::new()
            .with_local_provider("local")
            .with_tier(tier(Tier::Think, "anthropic", Some("deepseek")))
            .with_tier(tier(Tier::Build, "deepseek", Some("anthropic")))
            .with_tier(tier(Tier::Scan, "local", Some("deepseek")))
            .with_tier(tier(Tier::Reflex, "local", None)),
        Some("deepseek".to_owned()),
    )
    .with_provider(
        "anthropic",
        ProviderKind::Anthropic,
        "claude-opus-4",
        native(),
        ProviderHealth::Healthy,
    )
    .with_provider(
        "deepseek",
        ProviderKind::OpenaiCompatible,
        "deepseek-chat",
        native(),
        ProviderHealth::Healthy,
    )
    .with_provider(
        "local",
        ProviderKind::Local,
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
            location: None,
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
// 1. Structured-mode table routing + legible route_decided (BR-5, AC-3)
// --------------------------------------------------------------------------

#[tokio::test]
async fn structured_mode_routes_per_the_table_and_route_decided_names_the_binding() {
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
        let route = structured_route(&router, phase);
        assert_eq!(
            route.provider_id.as_ref().unwrap().0,
            provider,
            "phase {phase} maps to {} and routes to {provider}",
            category_for_phase(phase)
        );
        // BR-5: the reason names the binding that fired, by category and tier.
        let category = category_for_phase(phase);
        assert!(
            route.reason.contains(&format!("'{category}'")),
            "reason names the category that fired: {}",
            route.reason
        );
        // AC-9: the phase reached the route as attribution, never as dispatch —
        // the resolution the decision was made from carries no phase at all.
        assert_eq!(
            route.resolution.as_ref().map(|r| r.category),
            Some(category)
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
                // REQ-558 AC-8: every decision names its category and its tier.
                assert!(rd.category.is_some(), "route_decided carries a category");
                assert!(rd.tier.is_some(), "route_decided carries a tier");
                assert!(!rd.reason.is_empty(), "route_decided carries a reason");
            }
            other => panic!("expected route_decided, got {other:?}"),
        }
        assert_eq!(env.session_id.as_ref(), Some(&session));
    }
}

// --------------------------------------------------------------------------
// 2. Freeform reads the SAME table (REQ-558 BR-1) and emits route_decided (BR-5)
// --------------------------------------------------------------------------

#[tokio::test]
async fn freeform_decisions_read_the_configured_table_and_emit_route_decided() {
    let router = structured_router();
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-freeform");

    // **AC-1, the headline regression.** "explain the tradeoffs between these two
    // architectures" is a `design` turn, and `design` inherits `think` — so it
    // reaches the frontier provider bound there. The deleted `AUXILIARY_SIGNALS`
    // list sent that prompt to the 3B local model for containing "explain", and
    // never consulted the table at all.
    //
    // The prompt is not passed to the ROUTER here because it cannot be: `resolve`
    // takes a category. That absence is the fix; the classifier that does read the
    // prompt lives outside the router, and is exercised by
    // `the_route_classifier_sends_a_design_prompt_to_the_think_binding` below.
    let judgment = router.resolve(Category::Design);
    assert_eq!(
        judgment.provider_id.as_ref().unwrap().0,
        "anthropic",
        "a freeform `design` turn must reach the `think` binding, not the local \
         tier: {}",
        judgment.reason
    );
    assert!(judgment.phase.is_none(), "freeform carries no phase");
    router.emit_route_decided(&bus, Some(session.clone()), &judgment);

    // And a freeform turn with no classifier yet takes the BR-9 declared default
    // (`edit`), which inherits `build`.
    let coding = router.resolve(router.freeform_category());
    assert_eq!(coding.provider_id.as_ref().unwrap().0, "deepseek");
    router.emit_route_decided(&bus, Some(session.clone()), &coding);

    // BR-1 asserted directly: freeform and structured resolve the SAME category
    // through the SAME table to the same answer, byte for byte.
    let structured = structured_route(&router, CorePhase::Implement);
    assert_eq!(structured.provider_id, coding.provider_id);
    assert_eq!(structured.reason, coding.reason);

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
        assert!(rd.category.is_some(), "but it does carry a category (AC-8)");
        assert!(rd.tier.is_some());
        assert!(!rd.reason.is_empty(), "the decision carries a reason");
    }
    assert!(
        decided[0].reason.contains("'design'"),
        "the reason names the category that fired: {}",
        decided[0].reason
    );
    assert!(
        decided[1].reason.contains("'edit'"),
        "the reason names the category that fired: {}",
        decided[1].reason
    );
}

// --------------------------------------------------------------------------
// 2b. The `route` classifier: prompt → category → the SAME table (REQ-558 AC-1)
// --------------------------------------------------------------------------

/// **AC-1 through the public seam.** The classifier reads the prompt, the router
/// never does, and the answer meets the configured table at
/// [`Router::resolve_judgment`].
///
/// The prompt is the one from the requirement, verbatim. Against today's binary
/// it went to the 3B local model for containing the word `explain`; here it
/// reaches whatever the user bound to `think`.
#[tokio::test]
async fn the_route_classifier_sends_a_design_prompt_to_the_think_binding() {
    let router = structured_router();

    // The local tier answers the one-word contract. The engine is the local one
    // because `route` is pinned there by construction — `resolution_for` is what
    // says so, and it is the only availability question the classifier asks.
    let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::with_response(
        "qwen2.5-coder-3b",
        "design",
    )));
    let plan = classify::plan(
        &router.resolution_for(Category::Route),
        Some((engine, ChatFormat::Flat)),
    );
    let classification = classify::run(
        plan,
        "explain the tradeoffs between these two architectures",
        router.judgment_default(),
    )
    .await;

    assert_eq!(classification.category, JudgmentCategory::Design);
    assert_eq!(classification.signal, ClassificationSignal::Classified);

    let route = router.resolve_judgment(&classification);
    assert_eq!(
        route.provider_id.as_ref().unwrap().0,
        "anthropic",
        "a classified `design` turn must reach the `think` binding, not the local \
         tier: {}",
        route.reason
    );
    // BR-3: category, tier, provider, and the signal that fired, all on one event.
    let decided = route.route_decided().expect("a provider was selected");
    assert_eq!(decided.category, Some(teton_protocol::Category::Design));
    assert_eq!(decided.tier, Some(teton_protocol::Tier::Think));
    assert_eq!(decided.provider_id, ProviderId::from("anthropic"));
    assert!(decided.reason.contains("classifier"), "{}", decided.reason);
}

/// The BR-9 declared default is what a *bypassed* turn takes, and it is resolved
/// through the same chain rather than shortcut past it (LESSON-447). With the
/// local tier unable to serve, `route` resolves to nothing and no call is made.
#[tokio::test]
async fn an_unavailable_local_tier_bypasses_to_the_declared_default() {
    let router = structured_router().with_local_available(false);

    let engine: Arc<Mutex<dyn Engine>> =
        Arc::new(Mutex::new(MockEngine::with_response("local", "design")));
    let plan = classify::plan(
        &router.resolution_for(Category::Route),
        Some((engine, ChatFormat::Flat)),
    );
    let classification = classify::run(plan, "anything", router.judgment_default()).await;

    assert!(matches!(
        classification.signal,
        ClassificationSignal::Bypassed { .. }
    ));
    let route = router.resolve_judgment(&classification);
    // `edit` inherits `build`, bound to deepseek in this fixture.
    assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
    assert!(route.reason.contains("bypassed"), "{}", route.reason);
}

// --------------------------------------------------------------------------
// 3. Fallback on simulated provider failure completes the session (AC-7)
// --------------------------------------------------------------------------

#[tokio::test]
async fn simulated_provider_failure_falls_back_and_completes_emitting_provider_degraded() {
    // Implement primary = a flaky provider whose fallback is anthropic.
    let router = Router::new(
        CategoryTable::new()
            .with_local_provider("local")
            .with_tier(tier(Tier::Build, "flaky", Some("anthropic"))),
        Some("anthropic".to_owned()),
    )
    .with_provider(
        "flaky",
        ProviderKind::OpenaiCompatible,
        "flaky-model",
        native(),
        ProviderHealth::Healthy,
    )
    .with_provider(
        "anthropic",
        ProviderKind::Anthropic,
        "claude-opus-4",
        native(),
        ProviderHealth::Healthy,
    );

    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let session = SessionId::from("sess-ac7");

    // The primary is selected, then fails mid-turn with a fallback-class error.
    let primary = structured_route(&router, CorePhase::Implement);
    assert_eq!(primary.provider_id.as_ref().unwrap().0, "flaky");

    let outcome = router.on_provider_failure(&primary, "flaky", FailureClass::MalformedResponse);
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
    let route = structured_route(&router, CorePhase::Implement);
    let outcome = router.on_provider_failure(&route, "deepseek", FailureClass::MalformedToolCall);
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
// 4. Local tier unavailable → the category fails over rather than blocking (BR-8)
// --------------------------------------------------------------------------

#[tokio::test]
async fn a_category_bound_to_an_unavailable_local_tier_fails_over_rather_than_blocking() {
    // `digest` inherits `scan`, which this fixture binds to the local tier with a
    // remote fallback. With the tier available the turn stays local.
    let available = structured_router();
    assert_eq!(
        available
            .resolve(Category::Digest)
            .provider_id
            .as_ref()
            .unwrap()
            .0,
        "local"
    );

    // Below the hardware floor / gated / shed, the local tier cannot serve. REQ-544
    // BR-8's promise is that the loop does not block on it — and REQ-558 keeps that
    // promise through the *configured* fallback rather than a hardcoded bypass, so
    // the user can see and change where it goes.
    let router = structured_router().with_local_available(false);
    let route = router.resolve(Category::Digest);
    assert!(
        route.selected(),
        "BR-8: the router must not block on the local tier"
    );
    assert_eq!(
        route.provider_id.as_ref().unwrap().0,
        "deepseek",
        "failed over to the tier's configured fallback"
    );
    assert!(
        route.reason.contains("unavailable") && route.reason.contains("falling back"),
        "reason explains the failover: {}",
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
        CategoryTable::new()
            .with_local_provider("local")
            .with_tier(tier(Tier::Build, "kimi", None)),
        Some("kimi".to_owned()),
    )
    .with_provider(
        "kimi",
        ProviderKind::OpenaiCompatible,
        "kimi-k2",
        degraded(),
        ProviderHealth::Degraded,
    );

    let route = structured_route(&router, CorePhase::Implement);
    assert_eq!(route.provider_id.as_ref().unwrap().0, "kimi");

    // BR-6: reduced tool set, shorter loop, mandatory verification.
    assert!(route.harness.require_verification);
    assert_eq!(route.harness.max_tools, Some(5));
    assert!(route.harness.max_turns <= 5);
    // The degraded primary is kept (not failed over); the reason says so.
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

    // The spec phase maps to `design`, which routes to a remote (anthropic)
    // provider through its `think` binding.
    let route = structured_route(&router, CorePhase::Spec);
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
