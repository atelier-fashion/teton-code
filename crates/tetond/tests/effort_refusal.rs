//! REQ-559 BR-12 / AC-2b / AC-10: the effort refusal, end to end through the
//! egress choke point and the harness source.
//!
//! The unit tests in `teton-providers` prove the classification; these prove the
//! *behaviour it drives*, which is the part BR-12 actually specifies and the
//! part a classification test cannot see:
//!
//! - exactly **two** requests leave the daemon — the first stating its effort,
//!   the second stating none. Not one (the refusal was swallowed), not three
//!   (the fallback was itself retried).
//! - **no** request in the capture carries both reasoning shapes, at any point
//!   in the sequence (AC-10, AC-2b).
//! - the turn **completes** and returns content: a refusal is a degradation, not
//!   a turn failure.
//! - the source reports the refusal so the daemon can remember it for the
//!   session (ADR-F) rather than repeating a request known to fail.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::cost::ledger::CostLedger;
use tetond::cost::prices::PriceTable;
use tetond::egress::Egress;
use tetond::harness::turn_loop::run_session_turn_with_source;
use tetond::harness::{
    build_system_prompt, ContextManager, DutyRoute, HarnessConfig, NoopProvenanceHook,
    PendingPermissions, PermissionConfig, PermissionGate, RemoteProviderSource, SessionEvents,
    ToolContext, ToolDuties, ToolRegistry,
};

/// The 400 body a BYOM endpoint returns when it does not know
/// `reasoning_effort`. Verbatim in shape from the OpenAI-compatible family.
const EFFORT_400: &str = r#"{"error":{"message":"Unrecognized request argument supplied: reasoning_effort","type":"invalid_request_error"}}"#;

/// A transport that answers the **first** request with a 400 naming the effort
/// field and every later one with a normal streaming turn — the exact shape of
/// an endpoint that rejects the field and then works fine without it.
#[derive(Clone, Default)]
struct RefuseEffortOnce {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    calls: Arc<AtomicUsize>,
    replies: Arc<Mutex<VecDeque<String>>>,
}

impl RefuseEffortOnce {
    fn new() -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            replies: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Every request body, parsed. The tests assert on the parsed JSON rather
    /// than on substrings: a substring match on the serialized bytes would pass
    /// for a key nested somewhere harmless.
    fn bodies(&self) -> Vec<serde_json::Value> {
        self.sent
            .lock()
            .expect("capture lock")
            .iter()
            .map(|b| serde_json::from_slice(b).expect("adapters emit JSON bodies"))
            .collect()
    }
}

#[async_trait]
impl Transport for RefuseEffortOnce {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent
            .lock()
            .expect("capture lock")
            .push(request.body.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Ok(TransportResponse {
                location: None,
                status: 400,
                body: Box::pin(futures::stream::once(async move {
                    Ok(EFFORT_400.as_bytes().to_vec())
                })),
            });
        }
        let reply = self
            .replies
            .lock()
            .expect("reply lock")
            .pop_front()
            .unwrap_or_else(|| {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
                    "data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                )
                .to_owned()
            });
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(futures::stream::once(async move { Ok(reply.into_bytes()) })),
        })
    }
}

/// Whether a request body carries each reasoning shape.
fn shapes(body: &serde_json::Value) -> (bool, bool) {
    (
        body.get("reasoning_effort").is_some() || body.pointer("/output_config/effort").is_some(),
        body.get("thinking").is_some(),
    )
}

#[tokio::test]
async fn an_effort_refusal_costs_exactly_one_extra_request_and_never_both_shapes() {
    let transport = RefuseEffortOnce::new();
    let bus = Arc::new(EventBus::new());
    let ledger = CostLedger::open_in_memory(PriceTable::bundled(), bus.clone()).expect("ledger");
    let egress = Egress::new(transport.clone(), Vec::new(), bus.clone())
        .with_cost_meter(Arc::new(ledger.clone()));

    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "mystery",
        "https://byom.test/v1/chat/completions",
    ));
    let session_id = SessionId::from("refusal");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "mystery",
        "mystery-1",
        session_id.clone(),
        // Clamped, so the typed error has both levels to name (BR-12).
        ResolvedEffort::clamped(EffortLevel::Xhigh, EffortLevel::High),
    );

    let config = HarnessConfig::for_strong_model();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(std::env::temp_dir());
    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("hello");

    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        pending,
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());

    let mut hook = NoopProvenanceHook;
    let outcome = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    // A refusal is a degradation, not a turn failure (AC-10).
    assert!(outcome.is_ok(), "the turn must complete: {outcome:?}");

    let bodies = transport.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "exactly one retry: not zero (the refusal was swallowed) and not two \
         (the fallback was itself retried) — got {} requests",
        bodies.len(),
    );

    // The first states its effort — the ADR-E default reaching a BYOM endpoint,
    // which is the whole of AC-2b's first leg.
    assert_eq!(bodies[0]["reasoning_effort"], "high");
    // The second states none. Not a different level, not a thinking flag —
    // BR-12's fallback is the `none` shape, and "see which works" is exactly
    // what it forbids.
    let (effort, thinking) = shapes(&bodies[1]);
    assert!(
        !effort && !thinking,
        "the fallback must carry no reasoning field at all: {}",
        bodies[1],
    );

    // AC-10 / AC-2b: no request in the capture carries both, at any point.
    for (i, body) in bodies.iter().enumerate() {
        let (effort, thinking) = shapes(body);
        assert!(
            !(effort && thinking),
            "request {i} carried both shapes — a 400 on Kimi K2.5/K2.6: {body}",
        );
    }

    // The source reports the refusal so the daemon can remember it for the
    // session (ADR-F). Without this the fallback costs a doubled request on
    // every subsequent call for the life of the session.
    assert!(
        source.effort_was_refused(),
        "the refusal must be reported so the session memo can be seeded",
    );

    // And the refused request bought nothing, so it was not billed: exactly one
    // row for the two HTTP calls (the verify-phase regression this REQ found).
    let rows = ledger.all_records().expect("read the ledger");
    assert_eq!(
        rows.len(),
        1,
        "a refused request must not appear in the ledger, even though BR-12 \
         reads its body to classify it",
    );
}
