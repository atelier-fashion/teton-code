//! Cost-attribution acceptance harness (BR-2, AC-4 backend).
//!
//! The acceptance criterion for the cost ledger is that **every remote call
//! produces exactly one [`CostRecord`], attributed to the session's phase at
//! call time** — and that blocked or local calls produce none. So this test
//! wires the real [`Egress`] choke point in front of a **scripted transport**
//! (a mock returning provider-shaped usage instead of hitting the network),
//! installs a real in-memory [`CostLedger`] as the cost meter, drives a scripted
//! structured-mode session through the phase flow, and asserts:
//!
//! 1. One CostRecord per egress forward, with `(session, phase, provider, model)`
//!    and token counts read from the streamed usage.
//! 2. A privacy-blocked call is billed **zero** times (it never reaches the
//!    forward point).
//! 3. A call with no billing attribution is not recorded (a local-routed / probe
//!    call bills nothing — "none for local-tier inference").
//! 4. A repeated call (a retry) is recorded as its own record (BR-2: retries
//!    recorded individually).
//! 5. No ledger row carries prompt or credential content (BR-7): the row holds
//!    only token counts and routing metadata.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::CostRecord;
use teton_protocol::Phase;
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::cost::{CostAttribution, CostEventSink, CostLedger, PriceTable};
use tetond::egress::{Egress, EgressContext, EgressError, NoopSink, Provenance};

/// A distinctive secret the harness sends in request bodies; it must never turn
/// up in any ledger row (BR-7).
const SECRET: &str = "API_KEY=sk-live-DO-NOT-LEAK-abc123";

/// A `Transport` that returns a canned Anthropic-shaped SSE body carrying a
/// scripted `(input_tokens, output_tokens)` for each successive call, so a test
/// can tie each recorded row back to a known usage.
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

/// An Anthropic-style stream: input tokens in `message_start`, final output
/// tokens in the terminal `message_delta` (as the real adapter emits).
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

/// Captures every `cost_recorded` event the ledger emits.
#[derive(Default)]
struct CapturingCostSink {
    records: Mutex<Vec<CostRecord>>,
}

impl CostEventSink for CapturingCostSink {
    fn cost_recorded(&self, record: CostRecord) {
        self.records.lock().unwrap().push(record);
    }
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

fn ledger() -> (Arc<CostLedger>, Arc<CapturingCostSink>) {
    let sink = Arc::new(CapturingCostSink::default());
    let ledger = Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), sink.clone())
            .expect("open in-memory ledger"),
    );
    (ledger, sink)
}

#[tokio::test]
async fn every_egress_call_yields_exactly_one_attributed_cost_record() {
    // A structured-mode session marching through the phase flow; each phase makes
    // one remote call. Scripted usages let us tie each row to a known volume.
    let script = [(1200u64, 300u64), (900, 150), (5000, 2000), (1500, 600)];
    let transport = ScriptedTransport::with_script(&script);
    let (ledger, cost_sink) = ledger();
    let egress =
        Egress::new(transport, Vec::new(), Arc::new(NoopSink)).with_cost_meter(ledger.clone());

    // (phase, provider, model) for each call — implement routes to a cheap
    // provider, the frontier phases to Anthropic (AC-3 shape).
    let calls = [
        (Phase::Spec, "anthropic", "claude-opus-4"),
        (Phase::Architect, "anthropic", "claude-opus-4"),
        (Phase::Implement, "deepseek", "deepseek-chat"),
        (Phase::Review, "anthropic", "claude-opus-4"),
    ];

    for (phase, provider, model) in calls {
        let ctx = EgressContext::new(provider)
            .with_session("sess-alpha")
            .with_cost(CostAttribution::new(model).with_phase(phase));
        let resp = egress
            .send(request("prompt body here"), &Provenance::empty(), &ctx)
            .await
            .expect("clean call is allowed");
        // The caller must drain the stream (to read the turn completion); that is
        // when the metered body records the call.
        drain(resp.body).await;
    }

    let rows = ledger.all_records().expect("read rows");
    assert_eq!(
        rows.len(),
        calls.len(),
        "exactly one CostRecord per egress call"
    );

    // Each row's (phase, provider, model) and streamed token counts line up with
    // the scripted call, in order.
    for (i, ((phase, provider, model), (input, output))) in
        calls.iter().zip(script.iter()).enumerate()
    {
        assert_eq!(rows[i].session_id, "sess-alpha");
        assert_eq!(
            rows[i].phase,
            Some(*phase),
            "phase attribution at call time"
        );
        assert_eq!(rows[i].provider_id, *provider);
        assert_eq!(rows[i].model, *model);
        assert_eq!(
            rows[i].input_tokens, *input,
            "input read from streamed usage"
        );
        assert_eq!(
            rows[i].output_tokens, *output,
            "output read from streamed usage"
        );
    }

    // One `cost_recorded` event per row.
    assert_eq!(cost_sink.records.lock().unwrap().len(), calls.len());

    // The report attributes per phase, and the savings estimate is a labeled
    // estimate that reprices the same volume at the frontier baseline.
    let report = ledger.report().expect("report");
    let phases: Vec<&str> = report.per_phase.iter().map(|g| g.key.as_str()).collect();
    for expected in ["spec", "architect", "implement", "review"] {
        assert!(
            phases.contains(&expected),
            "per-phase attribution: {expected}"
        );
    }
    assert!(report.savings.is_estimate);
    assert!(
        report.savings.methodology.contains("Estimate"),
        "savings carries its methodology string (OQ-6)"
    );
    // Implement went to a cheaper provider than the Opus baseline, so there is a
    // strictly positive estimated saving.
    assert!(report.savings.savings_usd_micros > 0);
}

#[tokio::test]
async fn a_privacy_blocked_call_is_never_billed() {
    let transport = ScriptedTransport::with_script(&[(1000, 500)]);
    let (ledger, cost_sink) = ledger();
    let boundaries = vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }];
    let egress =
        Egress::new(transport, boundaries, Arc::new(NoopSink)).with_cost_meter(ledger.clone());

    // A call whose context provenance intersects a local-only boundary — even
    // with billing attribution attached — is refused before the forward point.
    let ctx = EgressContext::new("anthropic")
        .with_session("sess-blocked")
        .with_cost(CostAttribution::new("claude-opus-4").with_phase(Phase::Review));
    let err = egress
        .send(
            request(SECRET),
            &Provenance::tainted_by("secrets/prod.env"),
            &ctx,
        )
        .await
        .expect_err("boundary content must be blocked");
    assert!(matches!(err, EgressError::PrivacyBlocked { .. }));

    assert!(
        ledger.all_records().expect("read").is_empty(),
        "a blocked call must produce no CostRecord"
    );
    assert!(cost_sink.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_call_without_attribution_is_not_billed() {
    // Stands in for a local-routed / probe call: it flows through egress but the
    // caller attaches no CostAttribution, so nothing is recorded ("none for
    // local-tier inference").
    let transport = ScriptedTransport::with_script(&[(1000, 500)]);
    let (ledger, _sink) = ledger();
    let egress =
        Egress::new(transport, Vec::new(), Arc::new(NoopSink)).with_cost_meter(ledger.clone());

    let ctx = EgressContext::new("anthropic").with_session("sess-probe");
    let resp = egress
        .send(request("probe"), &Provenance::empty(), &ctx)
        .await
        .expect("allowed");
    drain(resp.body).await;

    assert!(
        ledger.all_records().expect("read").is_empty(),
        "an unattributed forward records nothing"
    );
}

#[tokio::test]
async fn a_retry_is_recorded_as_its_own_call() {
    // Two forwards of the same (session, phase) — a provider error then a retry —
    // are billed individually (BR-2).
    let transport = ScriptedTransport::with_script(&[(1000, 200), (1000, 210)]);
    let (ledger, _sink) = ledger();
    let egress =
        Egress::new(transport, Vec::new(), Arc::new(NoopSink)).with_cost_meter(ledger.clone());

    for _ in 0..2 {
        let ctx = EgressContext::new("anthropic")
            .with_session("sess-retry")
            .with_cost(CostAttribution::new("claude-opus-4").with_phase(Phase::Implement));
        let resp = egress
            .send(request("prompt"), &Provenance::empty(), &ctx)
            .await
            .expect("allowed");
        drain(resp.body).await;
    }

    let rows = ledger.all_records().expect("read");
    assert_eq!(rows.len(), 2, "each retry is its own CostRecord");
    assert_eq!(rows[0].output_tokens, 200);
    assert_eq!(rows[1].output_tokens, 210);
}

#[tokio::test]
async fn no_ledger_row_carries_prompt_or_credential_content() {
    // BR-7: the request body carries a secret; the ledger must store only token
    // counts and routing metadata, never content.
    let transport = ScriptedTransport::with_script(&[(1000, 500)]);
    let (ledger, cost_sink) = ledger();
    let egress =
        Egress::new(transport, Vec::new(), Arc::new(NoopSink)).with_cost_meter(ledger.clone());

    let ctx = EgressContext::new("anthropic")
        .with_session("sess-privacy")
        .with_cost(CostAttribution::new("claude-opus-4").with_phase(Phase::Spec));
    let resp = egress
        .send(request(SECRET), &Provenance::empty(), &ctx)
        .await
        .expect("allowed");
    drain(resp.body).await;

    let rows = ledger.all_records().expect("read");
    assert_eq!(rows.len(), 1);
    // Every stringy field is metadata only — no fragment of the secret anywhere.
    let row = &rows[0];
    for field in [&row.session_id, &row.provider_id, &row.model] {
        assert!(
            !field.contains("sk-live"),
            "no credential in ledger metadata"
        );
        assert!(!field.contains("API_KEY"));
    }
    assert_eq!(row.session_id, "sess-privacy");
    assert_eq!(row.model, "claude-opus-4");
    // The emitted event is equally content-free.
    let events = cost_sink.records.lock().unwrap();
    assert!(!events[0].model.contains("sk-live"));
}
