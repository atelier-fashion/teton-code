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
    category_for_phase, BindingSource, Category, CategoryOverride, CategoryTable,
    ConfigurableCategory, JudgmentCategory, Tier, TierBinding,
};
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::ProviderHealth;
use teton_core::ProvenanceId;
use teton_core::ToolCallTier;

use teton_inference::{ChatFormat, Engine, MockEngine};

use teton_protocol::events::{BudgetBound, Event};
use teton_protocol::{Phase as ProtoPhase, ProviderId, SessionId};

use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};
use teton_providers::{
    CapabilityProfile, FailureClass, Provider, ProviderError, StopReason, TokenUsage,
    TurnCompletion, TurnEvent, TurnRequest, TurnStream,
};

use tetond::broadcast::{EventBus, Subscription};
use tetond::classify::{self, ClassificationSignal};
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::{Egress, EgressError, NoopSink, Provenance};
use tetond::harness::budget::{derive, BudgetInputs};
use tetond::harness::turn_loop::HarnessConfig;
use tetond::harness::{DutyRoute, DRAFT_DUTY};
use tetond::repo_context::render::generated_header;
use tetond::router::{to_protocol_phase, Route, Router};

/// Mint the identity of a fixture file (REQ-571 ADR-A).
///
/// The provenance channel accepts only a [`ProvenanceId`], and an integration
/// test cannot reach the crate-internal fixture helper, so each test binary
/// states its own. A fixture naming a path that is not an identity is a broken
/// fixture, hence the panic.
fn source_id(path: &str) -> ProvenanceId {
    ProvenanceId::claimed(path).expect("fixture path must be a provenance id")
}

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
        // Above the local engine's own 32,768-token window, so the declared
        // window's pair is distinguishable from the default config's (which
        // derives from that window).
        max_context: 64_000,
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
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);
    let router = structured_router();
    let before = structured_route(&router, CorePhase::Implement);
    let outcome = router.on_provider_failure(&before, "deepseek", FailureClass::MalformedToolCall);
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

    // REQ-586 ADR-2 / AC-15: the profile degraded, the window did not. The
    // failure said this provider calls tools badly — not that it forgot how big
    // its context window is — so the continuing turn runs on the same budget it
    // was already running on, under the same bound, and nothing has to re-fit
    // the context mid-turn.
    assert_eq!(
        route.budget, before.budget,
        "the degrade keeps the failed provider's budget"
    );
    assert_eq!(route.budget.bound, BudgetBound::Window);
    assert_eq!(
        route.harness.budget, route.budget,
        "and the config the loop runs under carries the same fact (AC-12)"
    );
    assert_eq!(
        route.harness.context_budget_tokens,
        route.budget.budget_tokens
    );
    assert_eq!(
        route.route_decided().expect("provider kept").budget_tokens,
        before
            .route_decided()
            .expect("the failed route reported one")
            .budget_tokens,
        "the event after the degrade announces the budget it announced before"
    );

    // AC-15c, the negative half: **no `refit_on_reroute`**. The daemon's two
    // reroute arms re-budget and announce because they move the turn to a
    // different window; an in-place degrade moves it to a different *profile*,
    // and re-fitting a context that already fits — then telling the user their
    // context was re-fitted — would be a clamp that never happened.
    //
    // The load-bearing assertion is the pair equality below, and it is the
    // whole of it: byte-equal pairs are the condition `runtime::refit_for_reroute`
    // returns on, so the refit is **unreachable** rather than merely
    // unobserved. Watching this router's own degrade path for a
    // `context_pressure` would prove nothing — `emit_provider_degraded`
    // publishes one event and it is a `provider_degraded`; the router has no
    // code path that can publish context pressure under any mutation. The
    // silence of the *runtime* arm is a separate claim, pinned where that arm
    // lives, by
    // `runtime.rs::a_degrade_that_keeps_the_window_refits_nothing_and_says_nothing`.
    assert_eq!(
        (
            route.budget.budget_tokens,
            route.budget.budget_bytes,
            route.harness.context_budget_tokens,
            route.harness.context_budget_bytes,
        ),
        (
            before.budget.budget_tokens,
            before.budget.budget_bytes,
            before.harness.context_budget_tokens,
            before.harness.context_budget_bytes,
        ),
        "the pair moved, so the runtime would re-fit and announce a \
         degrade that changed no window"
    );
    // Non-vacuity: the degrade this test is about really was a degrade the
    // daemon would announce, rather than an outcome that fell through.
    router.emit_provider_degraded(&bus, None, degraded);
    let events = collect_events(&mut sub).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::ProviderDegraded(_))),
        "the degrade really was announced"
    );
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

    // REQ-586 BR-1/BR-8: a weak tool-caller is not a small-windowed one. `kimi`
    // declares a window, so the turn is budgeted from **that** window under
    // `bound: window` — the same inputs restated here and put through the one
    // derivation, so this asserts the router's classification rather than
    // re-doing its arithmetic.
    assert_eq!(route.budget.bound, BudgetBound::Window);
    assert_eq!(
        route.budget,
        derive(BudgetInputs {
            window: degraded().max_context,
            cap: 0,
            reservation: HarnessConfig::default().gen_params.max_tokens,
            is_local: false,
            redact_scan: false,
            provider_id: Some("kimi"),
            local_window: 0,
        }),
        "the route's budget is the declared window's, through `budget::derive`"
    );
    assert!(
        route.budget.budget_tokens > HarnessConfig::default().context_budget_tokens,
        "and the declared window actually moved the turn off the default pair"
    );
    // What the loop is held to, in both currencies, is that budget.
    assert_eq!(
        turn.config.context_budget_tokens,
        route.budget.budget_tokens
    );
    assert_eq!(turn.config.context_budget_bytes, route.budget.budget_bytes);
    assert_eq!(turn.config.budget, route.budget);
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
            origin: Default::default(),
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
            &Provenance::tainted_by(source_id("secrets/prod.env")),
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

// --------------------------------------------------------------------------
// 7. `draft` follows `think` by default and a policy row moves it (REQ-613
//    BR-4, AC-14)
// --------------------------------------------------------------------------

/// `teton policy set-category draft local`, as a table: `think` bound to a
/// healthy **remote** provider, and a `[[categories]]` row moving `draft` off
/// it — the second of AC-14's three fixtures.
///
/// A helper rather than a construction per test, so the case that asserts where
/// this fixture *resolves* and the case that asserts where it is *served* are
/// two questions about one fixture. Two hand-built tables would let the serving
/// case pass over a policy the resolution case never saw, which is the whole
/// failure mode AC-14 is written against.
fn draft_override_router() -> Router {
    Router::new(
        CategoryTable::new()
            .with_local_provider("local")
            .with_tier(tier(Tier::Think, "anthropic", None))
            .with_override(CategoryOverride {
                name: ConfigurableCategory::Draft,
                provider_id: "local".to_owned(),
                fallback_id: None,
            }),
        Some("anthropic".to_owned()),
    )
    .with_provider(
        "anthropic",
        ProviderKind::Anthropic,
        "claude-opus-4",
        native(),
        ProviderHealth::Healthy,
    )
    .with_provider(
        "local",
        ProviderKind::Local,
        "qwen",
        native(),
        ProviderHealth::Healthy,
    )
}

/// A machine with **no remote provider at all** — `think` itself is bound to
/// the local tier and there is no declared default behind it. AC-14's third
/// fixture, shared for [`draft_override_router`]'s reason.
fn offline_draft_router() -> Router {
    Router::new(
        CategoryTable::new()
            .with_local_provider("local")
            .with_tier(tier(Tier::Think, "local", None)),
        None,
    )
    .with_provider(
        "local",
        ProviderKind::Local,
        "qwen",
        native(),
        ProviderHealth::Healthy,
    )
}

/// **AC-14.** The repository-notes draft is a routed model call like any other:
/// it inherits whatever `think` names, a `[[categories]] name = "draft"` row
/// overrides that, and a machine with no remote provider drafts on the local
/// tier — with the resolver naming the tier in every case, because the header
/// ADR-5 writes into the generated file says which tier wrote it.
///
/// Driven through the router rather than through a `DaemonRuntime` for the
/// reason every test in this file is: what AC-14 claims is a *routing* fact, and
/// the router is where the decision is made. The daemon-side half — that the
/// draft duty asks this question at all — is the derived call-site marker in
/// `tetond::call_sites`.
///
/// # Mutations
///
/// Binding `Category::Draft` to `Tier::Scan` in `teton_core` sends the first
/// case to the `scan` provider and fails it; dropping `Draft` from
/// `ConfigurableCategory` makes the override unrepresentable and fails to
/// compile; making the resolver ignore per-category overrides for it leaves the
/// second case on `anthropic`.
#[tokio::test]
async fn draft_routes_to_the_think_binding_by_default_and_to_local_when_set() {
    // 1. The default. `think` is bound to a frontier provider, and `draft`
    //    inherits it because a file written once and read on every later turn is
    //    the one place the expensive model is the cheap choice (REQ-613 OQ-2).
    let router = structured_router();
    let route = router.resolve(Category::Draft);
    assert_eq!(
        route.provider_id.as_ref().expect("draft resolves").0,
        "anthropic",
        "draft must follow the `think` binding: {}",
        route.reason
    );
    assert_eq!(route.model.as_deref(), Some("claude-opus-4"));
    let resolution = router.resolution_for(Category::Draft);
    assert_eq!(resolution.tier, Tier::Think);
    assert_eq!(resolution.source, BindingSource::TierInheritance);
    assert!(
        route.reason.contains("draft"),
        "the reason names the category: {}",
        route.reason
    );
    // And it is not `design`'s decision borrowed: the two share a tier, and a
    // row below moves one without the other.
    assert_eq!(
        router.resolve(Category::Design).provider_id,
        route.provider_id
    );

    // 2. `teton policy set-category draft local` — a `[[categories]]` row like
    //    any other, and the resolver says the override is what fired.
    let local_draft = draft_override_router();
    let route = local_draft.resolve(Category::Draft);
    assert_eq!(
        route.provider_id.as_ref().expect("draft resolves").0,
        "local",
        "the policy row must move the draft: {}",
        route.reason
    );
    let resolution = local_draft.resolution_for(Category::Draft);
    assert_eq!(resolution.source, BindingSource::Override);
    assert_eq!(
        resolution.tier,
        Tier::Think,
        "the tier is a compile-time property; the row moves the provider, not the tier"
    );
    // The row is per category: `design` still goes where `think` says.
    assert_eq!(
        local_draft
            .resolve(Category::Design)
            .provider_id
            .expect("design resolves")
            .0,
        "anthropic"
    );

    // 3. No remote provider at all. The draft is served locally and the tier is
    //    still `think`, which is what ADR-5's header line reports.
    let offline = offline_draft_router();
    let route = offline.resolve(Category::Draft);
    assert_eq!(
        route.provider_id.as_ref().expect("draft resolves").0,
        "local",
        "with nothing remote the draft is written on the local tier: {}",
        route.reason
    );
    assert_eq!(offline.resolution_for(Category::Draft).tier, Tier::Think);
}

// --------------------------------------------------------------------------
// 8. ...and each of those three fixtures is *served* where it was sent
//    (REQ-613 BR-4, AC-14's second half)
// --------------------------------------------------------------------------

/// The session the drafting calls below are attributed to.
const DRAFT_SESSION: &str = "sess-draft-routing";

/// What the stand-in remote model answers a drafting call with.
const REMOTE_DRAFT: &str = "## Purpose\nA sample crate.\n";

/// A provider that really puts its request on the transport it was handed and
/// **drains the response**.
///
/// Both halves are load-bearing. The send is what makes the call a call; the
/// drain is what makes it a *billed* one, because the choke point meters a
/// request when its body is consumed — a provider that dropped the response
/// would leave the ledger empty and the row assertions below passing over a
/// call nobody made.
struct DraftingProvider;

#[async_trait]
impl Provider for DraftingProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> CapabilityProfile {
        CapabilityProfile::default()
    }

    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError> {
        let body = serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
        let response = transport
            .execute(TransportRequest {
                method: HttpMethod::Post,
                url: "https://api.anthropic.com/v1/messages".to_owned(),
                headers: Vec::new(),
                body,
            })
            .await
            .map_err(|_| ProviderError::Transport)?;
        drain(response.body).await;
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(TurnEvent::TextDelta(REMOTE_DRAFT.to_owned())),
            Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })),
        ])))
    }
}

/// The local tier serving a draft, answering `says`.
fn local_engine(model: &str, says: &str) -> Arc<Mutex<dyn Engine>> {
    Arc::new(Mutex::new(MockEngine::with_response(model, says)))
}

/// **AC-14's second half, on all three fixtures.** The case above settles where
/// the resolver *sends* a draft; this one settles that the route it named is the
/// route that **serves** one — the remote fixture on its provider, with a cost
/// row naming that provider and the draft category, and both local fixtures on
/// the local engine, with the tier the header line will carry.
///
/// A sibling rather than more assertions on the case above, because the two ask
/// different questions of different machinery: that one is about
/// `Router::resolve` and nothing else, this one spends its answer through the
/// duty seam. They share their fixtures ([`draft_override_router`],
/// [`offline_draft_router`]) so they cannot drift into being about two different
/// policies.
///
/// # Why the tier travels in a header line
///
/// ADR-5's generated file opens with the tier that wrote it, so "the tier names
/// it" is checked by composing that line from the resolution rather than by
/// reading the resolution twice — `generated_header` is imported, never
/// re-typed, so a change to the sentence cannot leave this assertion agreeing
/// with a header nobody ships (LESSON-456's rule, one layer up).
///
/// # Mutations
///
/// All three run 2026-09-03 and restored byte-identically.
///
/// 1. **Production.** Dropping `.with_category(..)` from `RemoteDuty`'s cost
///    attribution (`harness::duty`) leaves the row's `category` `None`, and the
///    remote leg fails on the assertion that `/cost` can name the draft.
/// 2. **The `served locally` claim is about which engine answered.** Serving
///    the override fixture with `DutyRoute::remote` in place of
///    `DutyRoute::local` answers with [`REMOTE_DRAFT`] instead of the engine's
///    line, and leg 2 fails — so "local" here is a fact about the answer, not a
///    restatement of the resolution the case above already made.
/// 3. **The ledger really is the instrument.** Dropping the `drain` in
///    [`DraftingProvider`] leaves the ledger empty (`one draft is one billed
///    call: []`), which is what a row assertion over an unmetered call would
///    have silently accepted had the assertion been on the row's contents
///    alone rather than on the count first.
#[tokio::test]
async fn each_draft_fixture_is_served_where_the_resolver_sent_it_and_the_row_and_header_name_it() {
    // --- 1. the default: `think` is remote, so the draft is drafted there ---
    let router = structured_router();
    let route = router.resolve(Category::Draft);
    let provider_id = route
        .provider_id
        .as_ref()
        .expect("the default fixture resolves the draft")
        .0
        .clone();
    let model = route
        .model
        .clone()
        .expect("a resolved binding names the model it will bill");
    assert_eq!(provider_id, "anthropic", "{}", route.reason);

    let (ledger, egress) = egress_with_ledger(
        ScriptedTransport::with_script(&[(4_100, 900)]),
        // No boundaries: what this case is about is *where* the draft went. The
        // covered-evidence half of BR-4 is `egress_capture.rs`'s, on the real
        // pipeline, where there is evidence to cover.
        Vec::new(),
    );
    // The provider and the model are the resolver's answers, not literals: a
    // duty built from a hardcoded provider would be served by whatever this test
    // typed rather than by whatever the policy named.
    let served = DutyRoute::remote(
        DRAFT_DUTY,
        provider_id.clone(),
        Box::new(DraftingProvider),
        egress,
        model.clone(),
        DRAFT_SESSION,
        ResolvedEffort::effort(EffortLevel::High),
    );
    assert_eq!(served.provider(), Some(provider_id.as_str()));
    let answer = served
        .perform("draft the notes", &Provenance::empty())
        .await
        .expect("the remote draft answers");
    assert_eq!(
        answer.trim(),
        REMOTE_DRAFT.trim(),
        "the answer must come from the provider the resolver named"
    );

    let rows = ledger.all_records().expect("read the ledger");
    assert_eq!(rows.len(), 1, "one draft is one billed call: {rows:?}");
    assert_eq!(
        rows[0].provider_id, "anthropic",
        "the row names the provider"
    );
    assert_eq!(rows[0].model, "claude-opus-4");
    assert_eq!(
        rows[0].category,
        Some(teton_protocol::Category::Draft),
        "the row names the draft category, which is what lets `/cost` show it"
    );
    assert_eq!(rows[0].phase, None, "a duty has no lifecycle position");
    assert_eq!(
        (rows[0].input_tokens, rows[0].output_tokens),
        (4_100, 900),
        "the counts tie the row to this call rather than to any call"
    );
    assert_eq!(rows[0].session_id, DRAFT_SESSION);
    // And the tier the file's first line will name is the resolution's own.
    assert!(
        generated_header(
            router.resolution_for(Category::Draft).tier.as_str(),
            "2026-09-03",
            None,
            None,
        )
        .contains("(think tier)"),
        "the header names the tier that served the draft"
    );

    // --- 2. `set-category draft local`: served by the local engine ----------
    //
    // The fixture has a healthy *remote* provider bound to `think`; the row is
    // the only reason this draft stays on the machine.
    let local_draft = draft_override_router();
    let route = local_draft.resolve(Category::Draft);
    let provider_id = route
        .provider_id
        .as_ref()
        .expect("the override fixture resolves the draft")
        .0
        .clone();
    assert_eq!(provider_id, "local", "{}", route.reason);
    let served = DutyRoute::local(
        DRAFT_DUTY,
        provider_id.clone(),
        local_engine("qwen", "drafted on the machine that asked"),
    );
    assert_eq!(served.provider(), Some("local"));
    let answer = served
        .perform("draft the notes", &Provenance::empty())
        .await
        .expect("the local draft answers");
    assert_eq!(
        answer.trim(),
        "drafted on the machine that asked",
        "the override must be served by the local engine, not by the remote \
         provider the same table still binds to `think`"
    );
    assert_eq!(
        ledger.all_records().expect("read the ledger").len(),
        1,
        "a locally served draft reaches no transport, so it bills nothing — the \
         only row in the ledger is still the remote leg's"
    );
    assert_eq!(
        local_draft.resolution_for(Category::Draft).tier,
        Tier::Think,
        "the row moved the provider, not the tier — and the tier is what the \
         header reports"
    );

    // --- 3. no remote provider at all: served locally, header still says ----
    let offline = offline_draft_router();
    let route = offline.resolve(Category::Draft);
    let provider_id = route
        .provider_id
        .as_ref()
        .expect("an offline machine still resolves a draft")
        .0
        .clone();
    assert_eq!(provider_id, "local", "{}", route.reason);
    let served = DutyRoute::local(
        DRAFT_DUTY,
        provider_id,
        // A different answer from leg 2's, so this leg cannot pass on that
        // leg's engine.
        local_engine("qwen", "drafted with nothing remote to reach"),
    );
    let answer = served
        .perform("draft the notes", &Provenance::empty())
        .await
        .expect("the offline draft answers");
    assert_eq!(answer.trim(), "drafted with nothing remote to reach");
    let header = generated_header(
        offline.resolution_for(Category::Draft).tier.as_str(),
        "2026-09-03",
        None,
        None,
    );
    assert!(
        header.contains("(think tier)"),
        "a machine with no remote provider still drafts on the `think` tier, and \
         the header says so: {header}"
    );
    assert_eq!(
        ledger.all_records().expect("read the ledger").len(),
        1,
        "nothing in this test billed a second call"
    );
}
