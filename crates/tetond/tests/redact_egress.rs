//! **REQ-562 acceptance matrix** — the redaction scan driven through the real
//! egress choke point, with the real two-pass scan behind it and the wire
//! captured (TASK-071).
//!
//! ## What this file adds that the unit tests do not
//!
//! TASK-070 pinned the gate hook at the choke point with a `CountingGate` — a
//! collaborator that returns a *canned* verdict. That is the right instrument
//! for "how many times was the scanner called" and for the decision arms, and it
//! is deliberately blind to everything between a payload and its verdict: no
//! router, no duty seam, no engine, no prompt, no parse.
//!
//! Everything here is assembled the way the daemon assembles it:
//!
//! - a **real [`Router`]**, resolving [`CoreCategory::Redact`] through the
//!   pinned-local branch, over a table whose `local_provider_id` is the value
//!   the daemon derives from the config;
//! - a **real [`DutyRoute`]**, built exactly as `RedactionGateImpl::redact_route`
//!   builds it (resolve, then the engine slot, else unresolved);
//! - the **real [`scan`]** — the deterministic pattern pass *and* the model pass,
//!   with a real [`Engine`] answering the real output contract;
//! - the **real [`Egress::send`]**, with the wire captured and every
//!   `privacy_block` recorded.
//!
//! So a claim here is a claim about the bytes that would have left the machine,
//! not about a mock's return value (LESSON-432, LESSON-485).
//!
//! ## The one thing that is a copy, and why it is bounded
//!
//! `RedactionGateImpl` is private to `tetond`, so [`TestGate`] below restates
//! its two-line body. What that leaves outside this file is exactly one step:
//! the derivation of `local_provider_id` from a config (`runtime::local_tier_id`,
//! BUG-156/TASK-057), which is pinned by
//! `runtime::tests::the_local_tier_id_is_never_a_registered_remote_providers_id`
//! and by `runtime::tests::dispatch::redact::a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote`.
//! Everything downstream of that one value is real here — including, for AC-5,
//! a gate deliberately given the ability to route the scan over HTTP, so the
//! capture has something it could catch.
//!
//! ## AC-1's wording versus BR-4's rule (reported, not papered over)
//!
//! AC-1 says a credential "the model paraphrased into prose" is **blocked**.
//! BR-4, ADR-4 and AC-10 say the opposite for that exact input: confidence is
//! *derived* — a pattern hit is `High` and blocks, a model-only hit is `Low` and
//! is the user's call — and AC-10 requires in as many words that "a
//! low-confidence-only payload is not blocked". The architecture adjudicated in
//! favour of BR-4/AC-10 (ADR-4's decision table), and this task's own AC-9 asks
//! for byte-identity on a **Low-only-forward** leg, which only exists if a
//! low-only verdict forwards.
//!
//! So AC-1 is asserted here as the two legs the shipped design has:
//! [`a_planted_credential_with_clean_provenance_is_blocked_and_the_event_names_redaction`]
//! (the blocking leg — a pattern-shaped sentinel provenance cannot see) and
//! [`a_credential_the_model_paraphrased_is_located_and_reported_without_blocking`]
//! (the leg AC-1's prose is about — the capability patterns structurally cannot
//! provide). The contradiction is in the *requirement prose*, not in the code,
//! and it is recorded rather than resolved by an edit (LESSON-487).

use std::sync::{Arc, Mutex};
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::ProviderKind;

use async_trait::async_trait;
use futures::stream;

use teton_core::category::{Category as CoreCategory, CategoryTable};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::policy::ProviderHealth;
use teton_core::ProvenanceId;
use teton_inference::{Completion, Engine, EngineError, GenParams};
use teton_protocol::events::{
    BlockCause, BudgetBound, Event, FindingKind as WireFindingKind, PrivacyAction, PrivacyBlock,
    ProvenanceRejected,
};
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};
use teton_providers::{
    CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, TurnCompletion, TurnEvent,
    TurnRequest, TurnStream,
};

use tetond::egress::provenance::{assembled_provenance, ContextBlock};
use tetond::egress::redact::{
    decide, EgressDecision, Outcome, RedactionGate, RedactionVerdict, REDACT_CHUNK_MAX_BYTES,
    REDACT_INPUT_MAX_BYTES, REDACT_SCANNABLE_CONTEXT_BYTES,
};
use tetond::egress::{Egress, EgressContext, EgressError, NoopSink, PrivacyEventSink, Provenance};
use tetond::harness::budget::derive;
use tetond::harness::redact::{scan, REDACTION_OUTPUT_CONTRACT};
use tetond::harness::{BudgetInputs, DutyRoute, REDACT_DUTY};
use tetond::router::Router;

/// Mint the identity of a fixture file (REQ-571 ADR-A).
///
/// The provenance channel accepts only a [`ProvenanceId`], and an integration
/// test cannot reach the crate-internal fixture helper, so each test binary
/// states its own. A fixture naming a path that is not an identity is a broken
/// fixture, hence the panic.
fn source_id(path: &str) -> ProvenanceId {
    ProvenanceId::claimed(path).expect("fixture path must be a provenance id")
}

// ===========================================================================
// Sentinels
// ===========================================================================

/// A credential **shaped like one of the five pattern shapes**, so the
/// deterministic pass finds it with no model involved (ADR-4).
///
/// Distinctive enough that its appearance anywhere outside a forwarded payload
/// is unambiguous rather than inferred — which is the whole instrument AC-6
/// uses.
const PATTERN_SENTINEL: &str = "AKIASENTINEL562ABCDE";

/// A credential **no pattern can see**: the model is told it is one, and the
/// redactor locates the string it quoted (ADR-5).
///
/// Deliberately ordinary-looking. It is the input AC-1's prose is about, and the
/// only thing that can catch it is the model pass.
const PARAPHRASED_SENTINEL: &str = "the deploy password is orange-walrus-9";

/// Content that lives only inside a `local-only` file, for the ordering leg.
const BOUNDARY_SECRET: &str = "postgres://prod-db.internal/teton";

// ===========================================================================
// The wire
// ===========================================================================

/// A `Transport` that records every request instead of sending it.
///
/// Cloneable with a shared record, so the same wire can sit under the turn's
/// choke point **and** under a duty's — which is what makes "zero
/// scan-originated requests" a statement about one wire rather than two.
#[derive(Default, Clone)]
struct CaptureTransport {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
}

impl CaptureTransport {
    fn captured(&self) -> Vec<TransportRequest> {
        self.sent.lock().expect("capture poisoned").clone()
    }

    /// Every captured body as text — the haystack for the AC-6 sweep.
    fn bodies(&self) -> Vec<String> {
        self.captured()
            .into_iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect()
    }

    /// Captured requests that carry a **redaction prompt** — i.e. requests the
    /// scan itself originated.
    ///
    /// Recognized by the duty's own output contract, which is the one string
    /// every redact prompt contains and nothing else in this file writes. AC-5
    /// is asserted on this count, never on a provider id (BUG-156, LESSON-485).
    fn scan_originated(&self) -> usize {
        self.bodies()
            .iter()
            .filter(|b| b.contains(REDACTION_OUTPUT_CONTRACT))
            .count()
    }
}

#[async_trait]
impl Transport for CaptureTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().expect("capture poisoned").push(request);
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(stream::once(async { Ok(b"{\"ok\":true}".to_vec()) })),
        })
    }
}

/// Captures every `privacy_block` the choke point emits.
#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<(Option<SessionId>, PrivacyBlock)>>,
}

impl CapturingSink {
    fn events(&self) -> Vec<(Option<SessionId>, PrivacyBlock)> {
        self.events.lock().expect("sink poisoned").clone()
    }

    fn only(&self) -> PrivacyBlock {
        let mut events = self.events();
        assert_eq!(events.len(), 1, "expected exactly one privacy_block");
        events.pop().expect("checked above").1
    }
}

impl PrivacyEventSink for CapturingSink {
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock) {
        self.events
            .lock()
            .expect("sink poisoned")
            .push((session_id, block));
    }
    // Required no-op: this AC-9 fixture captures blocks, not rejections.
    fn provenance_rejected(&self, _session_id: Option<SessionId>, _rejected: ProvenanceRejected) {}
}

/// A provider that serializes its `TurnRequest` onto the transport — the
/// adversarial half of AC-5's fixture, so a scan routed remotely leaves a
/// recognizable body on the captured wire.
struct WireProvider;

#[async_trait]
impl Provider for WireProvider {
    fn id(&self) -> &str {
        "wire"
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
        transport
            .execute(TransportRequest {
                method: HttpMethod::Post,
                url: "https://squatter.example.com/v1/chat/completions".to_owned(),
                headers: Vec::new(),
                body,
            })
            .await
            .map_err(|err| match err {
                TransportError::PrivacyBlocked(detail) => ProviderError::PrivacyBlocked(detail),
                _ => ProviderError::Transport,
            })?;
        Ok(Box::pin(stream::iter(vec![
            Ok(TurnEvent::TextDelta("NONE".to_owned())),
            Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })),
        ])))
    }
}

// ===========================================================================
// The engine
// ===========================================================================

/// A shared prompt log, and the erased engine handle that feeds it.
///
/// Two handles onto one object: the `Arc<Mutex<dyn Engine>>` the duty seam
/// wants, and a probe the test reads the count off. Kept as a pair because an
/// `Arc<Mutex<dyn Engine>>` cannot be downcast back to its concrete type.
///
/// The log is the **model-call count** for the redaction duty: a redact prompt
/// is the one carrying [`REDACTION_OUTPUT_CONTRACT`], so [`Self::redact_calls`]
/// counts scans that actually reached a model rather than scans that were
/// short-circuited (over-cap, unresolved route). Both numbers matter and they
/// are not the same number — which is exactly what
/// [`a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call`]
/// turns on.
#[derive(Clone)]
struct EnginePair {
    engine: Arc<Mutex<dyn Engine>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl EnginePair {
    /// Prompts carrying the redaction contract — model calls the scan made.
    fn redact_calls(&self) -> usize {
        self.prompts
            .lock()
            .expect("prompt log poisoned")
            .iter()
            .filter(|p| p.contains(REDACTION_OUTPUT_CONTRACT))
            .count()
    }
}

/// A recording engine answering `reply`, with a probe onto its prompt log.
fn engine_pair(reply: &str) -> EnginePair {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(Mutex::new(LoggingEngine {
        reply: reply.to_owned(),
        prompts: Arc::clone(&prompts),
    })) as Arc<Mutex<dyn Engine>>;
    EnginePair { engine, prompts }
}

/// A local [`Engine`] that answers with `reply` and appends every prompt it was
/// given to a log the test also holds.
struct LoggingEngine {
    reply: String,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl Engine for LoggingEngine {
    fn model_id(&self) -> &str {
        "redact-acceptance-local"
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        self.prompts
            .lock()
            .expect("prompt log poisoned")
            .push(prompt.to_owned());
        let mut text = String::new();
        let mut completion_tokens = 0u32;
        for token in self.reply.split_inclusive(' ') {
            if completion_tokens >= params.max_tokens {
                break;
            }
            let keep_going = on_token(token);
            text.push_str(token);
            completion_tokens += 1;
            if !keep_going {
                break;
            }
        }
        Ok(Completion::cold(
            text,
            u32::try_from(prompt.len() / 4).unwrap_or(u32::MAX),
            completion_tokens,
        ))
    }
}

// ===========================================================================
// The gate — `RedactionGateImpl`, rebuilt from public seams
// ===========================================================================

/// How a resolved provider id that the engine slot cannot serve is handled.
type RemoteArm = Box<dyn Fn(&str) -> DutyRoute + Send + Sync>;

/// The daemon's redaction gate: resolve the category, build the route, run the
/// scan, hand back the verdict.
///
/// Line for line what `RedactionGateImpl` does, plus two instruments and one
/// deliberate weakness:
///
/// - `scans` counts entries into [`RedactionGate::scan`] — the number AC-2,
///   AC-11 and AC-12 are about, and *not* the same as the number of model calls
///   (a short-circuited scan enters and issues none);
/// - `verdicts` keeps what the choke point acted on, so a test can assert on the
///   confidence level behind a forward rather than only on the forward;
/// - `remote_arm` is the construction production **does not have**. The real
///   gate holds no transport, no provider and no ledger, so the only route it
///   can build is a local one. AC-5's fixture installs an arm that *can* build a
///   remote route, because a capture assertion that nothing was dispatched is
///   worth nothing if nothing could have been (LESSON-485).
struct TestGate {
    router: Router,
    engine: Option<Arc<Mutex<dyn Engine>>>,
    remote_arm: Option<RemoteArm>,
    scans: Mutex<Vec<String>>,
    verdicts: Mutex<Vec<RedactionVerdict>>,
}

impl TestGate {
    fn new(router: Router) -> Self {
        Self {
            router,
            engine: None,
            remote_arm: None,
            scans: Mutex::new(Vec::new()),
            verdicts: Mutex::new(Vec::new()),
        }
    }

    fn serving(mut self, engine: Arc<Mutex<dyn Engine>>) -> Self {
        self.engine = Some(engine);
        self
    }

    fn with_remote_arm(mut self, arm: RemoteArm) -> Self {
        self.remote_arm = Some(arm);
        self
    }

    fn calls(&self) -> usize {
        self.scans.lock().expect("scan log poisoned").len()
    }

    fn verdicts(&self) -> Vec<RedactionVerdict> {
        self.verdicts.lock().expect("verdict log poisoned").clone()
    }

    fn only_verdict(&self) -> RedactionVerdict {
        let mut verdicts = self.verdicts();
        assert_eq!(verdicts.len(), 1, "expected exactly one scan");
        verdicts.pop().expect("checked above")
    }

    /// `RedactionGateImpl::redact_route`, restated.
    fn redact_route(&self) -> DutyRoute {
        let route = self.router.resolve(CoreCategory::Redact);
        let Some(provider_id) = route.provider_id.as_ref().map(|p| p.0.clone()) else {
            return DutyRoute::unresolved(route.reason.clone());
        };
        if let Some(engine) = &self.engine {
            return DutyRoute::local(REDACT_DUTY, provider_id, Arc::clone(engine));
        }
        if let Some(arm) = &self.remote_arm {
            return arm(&provider_id);
        }
        DutyRoute::unresolved(format!(
            "The 'redact' category resolves to '{provider_id}', but no local engine is loaded \
             to serve it yet."
        ))
    }
}

#[async_trait]
impl RedactionGate for TestGate {
    async fn scan(&self, payload: &str) -> RedactionVerdict {
        self.scans
            .lock()
            .expect("scan log poisoned")
            .push(payload.to_owned());
        let verdict = scan(&self.redact_route(), payload).await;
        self.verdicts
            .lock()
            .expect("verdict log poisoned")
            .push(verdict.clone());
        verdict
    }
}

// ===========================================================================
// Fixtures
// ===========================================================================

/// The canonical local-tier id, spelled here because a fixture has to name a
/// provider. **Nothing under test compares against it** — that is the whole of
/// BR-2, and [`an_engine_backed_local_tier_under_another_id_still_serves_the_scan`]
/// is what would catch a comparison creeping in.
const CANONICAL_LOCAL_ID: &str = "local";

/// A router whose local tier is `local_provider_id`, or which has none.
///
/// `local_provider_id` is the value `runtime::local_tier_id` derives from the
/// config: the id of a provider declaring `kind = "local"`, or the canonical id
/// while nothing else answers to it, or **`None`** when a non-local provider has
/// taken that name (BUG-156/TASK-057).
fn router_with_local_tier(local_provider_id: Option<&str>) -> Router {
    let mut table = CategoryTable::new();
    table.local_provider_id = local_provider_id.map(str::to_owned);
    let mut router = Router::new(table, Some("frontier".to_owned())).with_provider(
        "frontier",
        ProviderKind::OpenaiCompatible,
        "claude-opus-4",
        CapabilityProfile::default(),
        ProviderHealth::Healthy,
    );
    if let Some(id) = local_provider_id {
        router = router.with_provider(
            id,
            ProviderKind::OpenaiCompatible,
            "on-device-3b",
            CapabilityProfile::default(),
            ProviderHealth::Healthy,
        );
    }
    router
}

fn local_only_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

/// Assemble a request the way the daemon does: serialize the context blocks into
/// a body and derive the request's provenance from them.
fn assemble(blocks: &[ContextBlock]) -> (TransportRequest, Provenance) {
    let joined = blocks
        .iter()
        .map(ContextBlock::content)
        .collect::<Vec<_>>()
        .join("\n");
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: joined.into_bytes(),
    };
    (request, assembled_provenance(blocks))
}

/// A payload assembled from clean, public sources — **no boundary can catch
/// this**, which is the premise every redaction claim rests on.
fn clean_provenance_payload(text: &str) -> (TransportRequest, Provenance) {
    assemble(&[
        ContextBlock::synthetic("You are Teton Code."),
        ContextBlock::from_file(source_id("src/main.rs"), text),
    ])
}

/// The choke point under test: capture beneath it, a recording sink beside it,
/// boundaries armed, and `gate` installed.
fn choke_point(
    capture: &CaptureTransport,
    sink: &Arc<CapturingSink>,
    gate: Arc<TestGate>,
) -> Egress<CaptureTransport> {
    Egress::new(capture.clone(), local_only_boundaries(), sink.clone())
        .with_redaction_gate(gate as Arc<dyn RedactionGate>)
}

fn ctx() -> EgressContext {
    EgressContext::new("anthropic").with_session("sess-redacted")
}

/// A model reply in the duty's own contract shape, quoting `quoted`.
fn model_reports(kind: &str, quoted: &str) -> String {
    format!("{kind}: {quoted}")
}

// ===========================================================================
// AC-1 — the capability the REQ exists for
// ===========================================================================

/// **AC-1, the blocking leg** (and AC-11's ordering, and AC-6's first surface).
///
/// A credential with *clean provenance* and *no matching boundary glob*: the
/// provenance inspection has nothing to catch it by, and it is refused anyway.
/// That is the whole claim of the REQ, and it fails against a build without the
/// gate.
///
/// Four things are asserted together because each alone is satisfiable by a
/// broken build: the send is refused, the cause names **redaction** (not a
/// boundary), the wire saw **nothing**, and the emitted event carries the kind
/// and span but not a byte of the payload.
///
/// The second half of the test is AC-11: the same choke point, the same gate,
/// a payload the *boundary* refuses — and the scanner is never called, because
/// spending an inference on content that was never allowed to leave is work done
/// for a call that cannot happen.
#[tokio::test]
async fn a_planted_credential_with_clean_provenance_is_blocked_and_the_event_names_redaction() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) = clean_provenance_payload(&format!(
        "// deploy note\nlet key = \"{PATTERN_SENTINEL}\";\n"
    ));
    // The premise: nothing about *where this came from* is refusable.
    assert!(
        provenance.sources().all(|p| !p.starts_with("secrets/")),
        "the fixture must carry clean provenance, or the block below proves nothing"
    );

    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("a planted credential must not leave the machine");

    match err {
        EgressError::PrivacyBlocked {
            ref path,
            ref provider_id,
            action,
            cause: BlockCause::Redaction { kind, span },
        } => {
            assert_eq!(kind, WireFindingKind::Credential);
            assert_eq!(provider_id, &ProviderId::from("anthropic"));
            assert_eq!(action, PrivacyAction::ReroutedToLocal);
            assert_eq!(path, "the outbound payload");
            assert!(span.end > span.start, "a finding must locate itself");
        }
        other => panic!("expected a redaction block, got {other:?}"),
    }

    assert!(
        capture.captured().is_empty(),
        "a blocked payload must reach no wire at all"
    );

    let block = sink.only();
    assert_eq!(block.provider_id, ProviderId::from("anthropic"));
    assert!(
        matches!(block.cause, BlockCause::Redaction { .. }),
        "the event and the typed error must name the same inspection: {:?}",
        block.cause
    );

    // The scan really ran — both passes — rather than the block coming from a
    // scan that could not run (which is a different cause entirely).
    let verdict = gate.only_verdict();
    assert_eq!(verdict.outcome(), Outcome::Findings);
    assert!(verdict.scanned(), "a completed scan says so");
    assert_eq!(engine.redact_calls(), 1, "the model pass ran too");

    // --- AC-11: the ordering, by scanner call count ------------------------
    let (refused, refused_provenance) = assemble(&[
        ContextBlock::synthetic("You are Teton Code."),
        ContextBlock::from_file(source_id("secrets/prod.env"), BOUNDARY_SECRET),
    ]);
    let err = egress
        .send(refused, &refused_provenance, &ctx())
        .await
        .expect_err("boundary content must still be refused");
    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::Boundary,
                ..
            }
        ),
        "provenance refuses first, and says so: {err:?}"
    );
    assert_eq!(
        gate.calls(),
        1,
        "a payload provenance already refused is never handed to the scanner"
    );
}

/// **AC-1's other leg — the capability patterns structurally cannot provide.**
///
/// The payload carries a credential *described in prose*. No pattern shape
/// matches it; the model reports it, the redactor locates the quoted string in
/// the payload, and the finding is `Low` **by construction** (ADR-4 — the
/// model's own certainty is never consulted).
///
/// It therefore **forwards**, per BR-4 and AC-10, and this test asserts that
/// rather than AC-1's prose. See this file's module docs: the requirement's AC-1
/// and its AC-10 disagree about this input, the architecture adjudicated in
/// favour of AC-10, and the disagreement is reported rather than smoothed over.
///
/// It doubles as **AC-9's Low-only-forward leg**: a scan that found something
/// and forwarded anyway must forward the *original* bytes. v1 detects; it never
/// substitutes.
#[tokio::test]
async fn a_credential_the_model_paraphrased_is_located_and_reported_without_blocking() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    // The stand-in the daemon ships answers "NONE" and cannot judge sensitivity,
    // so a model *catch* needs an engine with a canned contract-shaped reply.
    let engine = engine_pair(&model_reports("credential", PARAPHRASED_SENTINEL));
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) = clean_provenance_payload(&format!(
        "Here is the runbook. Note that {PARAPHRASED_SENTINEL}, so be careful."
    ));
    let sent_bytes = request.body.clone();
    // The scan input is the *whole outbound body* read as text (ADR-1), so that
    // is what a span indexes — not the file block the fixture pasted in.
    let payload = String::from_utf8_lossy(&sent_bytes).into_owned();

    // The premise, asserted rather than assumed: no pattern shape sees this.
    assert!(
        tetond::egress::redact::pattern_pass(&payload).is_empty(),
        "the paraphrase must be invisible to the deterministic pass, or the \
         model pass is not what caught it"
    );

    let response = egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a low-confidence finding is the user's call, not a block (BR-4)");
    assert_eq!(response.status, 200);

    let verdict = gate.only_verdict();
    assert_eq!(
        verdict.outcome(),
        Outcome::Findings,
        "the model located a credential prose alone would have hidden"
    );
    assert_eq!(verdict.findings().len(), 1);
    let finding = &verdict.findings()[0];
    assert!(
        !finding.is_high(),
        "a model-only hit is Low by construction (ADR-4)"
    );
    assert_eq!(
        &payload[finding.span().clone()],
        PARAPHRASED_SENTINEL,
        "the span must point at the thing the model reported, in the bytes that \
         would have gone on the wire"
    );
    assert_eq!(decide(&verdict), EgressDecision::Forward);
    assert!(sink.events().is_empty(), "a forward emits no privacy_block");

    // AC-9, the Low-only leg: byte-for-byte what was assembled.
    let captured = capture.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].body, sent_bytes,
        "a scan that found something and forwarded anyway must not have touched \
         the bytes (BR-5)"
    );

    // **AC-1's "reported" half, on the real verdict.** A finding that forwards
    // is still a finding, and it reaches the daemon's log as kind + span. The
    // lines are built here from the same verdict `Egress::send` reports from —
    // `send` writes them to stderr, which is `tetond.log` and which no in-process
    // test can capture; the call itself is pinned by
    // `egress::tests::the_gate_arm_reports_a_forwarded_findings_verdict`.
    let report = tetond::egress::redact::forwarded_findings_report(&verdict);
    assert_eq!(report.len(), 1, "one located finding, one line: {report:?}");
    assert!(
        report[0].contains("low-confidence") && report[0].contains("credential"),
        "the line must carry the confidence and the kind: {}",
        report[0]
    );
    assert!(
        report[0].contains(&format!("{}-{}", finding.span().start, finding.span().end)),
        "and the span, which is the only thing that says where to look: {}",
        report[0]
    );
    assert!(
        !report[0].contains(PARAPHRASED_SENTINEL) && !report[0].contains("orange-walrus"),
        "BR-6 VIOLATION — the matched text reached the daemon log: {}",
        report[0]
    );
}

// ===========================================================================
// AC-2 / AC-9 — the clean payload, and the proof the scan ran
// ===========================================================================

/// **AC-2 with its non-vacuity pairing, and AC-9's clean-forward leg.**
///
/// A clean payload passes — and "passes" is only interesting alongside evidence
/// that something looked: exactly one entry into the gate, exactly one model
/// call, and `scanned: true` on the verdict. Without those three a build that
/// skipped the scan entirely would pass this test (LESSON-485).
#[tokio::test]
async fn a_clean_payload_forwards_and_the_scan_provably_ran() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) =
        clean_provenance_payload("fn main() { println!(\"nothing to see\"); }");
    let sent_bytes = request.body.clone();

    let response = egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a clean payload forwards");
    assert_eq!(response.status, 200);

    // Non-vacuity: it forwarded *because it was scanned and found clean*.
    assert_eq!(gate.calls(), 1, "the scanner was entered exactly once");
    assert_eq!(engine.redact_calls(), 1, "and it reached the model");
    let verdict = gate.only_verdict();
    assert_eq!(verdict.outcome(), Outcome::Clean);
    assert!(
        verdict.scanned(),
        "a clean verdict must claim the scan it actually ran"
    );

    // AC-9, the clean leg.
    let captured = capture.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].body, sent_bytes,
        "the forwarded bytes must be the assembled bytes, unmodified"
    );
    assert!(sink.events().is_empty());
}

// ===========================================================================
// AC-3 — fail closed, and say which failure it was
// ===========================================================================

/// **AC-3.** `[privacy] redact` is on and this machine has no local tier able to
/// serve it: the route is unresolved, the verdict is `Unavailable`, the payload
/// is **blocked**, and no surface claims a scan happened.
///
/// The payload is deliberately **clean** — the one input that would forward if
/// an unrunnable guard were treated permissively, so a permissive regression
/// turns this red rather than leaving it green (LESSON-447).
///
/// The third assertion is the one AC-3 adds over "it blocked": the serialized
/// event says `scan_unavailable` and nothing anywhere says the content was
/// scanned. A block that could not say *why* sends the user hunting for a secret
/// that is not there instead of for the tier that is not loaded (BR-3).
#[tokio::test]
async fn with_no_local_tier_the_payload_is_blocked_unscanned_and_nothing_claims_otherwise() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    // A router with no local tier at all — the remote-only machine with the
    // switch on. No engine, and no arm that could route the scan elsewhere.
    let gate = Arc::new(TestGate::new(router_with_local_tier(None)));
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) = clean_provenance_payload("entirely ordinary prose");
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("a guard that cannot run does not become a guard that passes");

    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::ScanUnavailable,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        capture.captured().is_empty(),
        "AC-3 by captured bytes: nothing was sent"
    );

    let verdict = gate.only_verdict();
    assert_eq!(verdict.outcome(), Outcome::Unavailable);
    assert!(
        !verdict.scanned(),
        "a scan that could not run must never claim it did"
    );
    assert!(verdict.findings().is_empty());

    let block = sink.only();
    assert_eq!(block.cause, BlockCause::ScanUnavailable);
    let json = serde_json::to_string(&Event::PrivacyBlock(block)).expect("serialize event");
    assert!(json.contains("scan_unavailable"), "{json}");
    assert!(
        !json.contains("\"scanned\":true") && !json.contains("redaction"),
        "no configuration may make an unscanned payload look scanned: {json}"
    );

    // And the sentence a person reads says which of the two it was.
    let rendered = err.to_string();
    assert!(rendered.contains("could not run"), "{rendered}");
    assert!(
        !rendered.contains("found"),
        "nothing looked, so nothing was found: {rendered}"
    );
}

/// **AC-3's second door — BR-7's bound.** A payload past
/// [`REDACT_INPUT_MAX_BYTES`] is the same "could not run" case, on a machine
/// whose tier is loaded and working.
///
/// It is not truncated and scanned: a scan of the first half of a payload that
/// reports the payload clean is the lie BR-7 forbids. And it costs **zero**
/// model calls, which is what tells a short-circuit from a scan that ran and
/// found nothing.
#[tokio::test]
async fn a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let oversized = "x".repeat(REDACT_INPUT_MAX_BYTES + 1);
    let (request, provenance) = clean_provenance_payload(&oversized);
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("an unscannable payload is blocked, not waved through");

    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::ScanUnavailable,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(capture.captured().is_empty());
    assert_eq!(gate.calls(), 1, "the gate was entered");
    assert_eq!(
        engine.redact_calls(),
        0,
        "and it short-circuited before spending an inference"
    );

    // Non-vacuity: the same choke point, one byte under the cap, forwards.
    let under = "y".repeat(1_024);
    let (request, provenance) = clean_provenance_payload(&under);
    egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a payload under the cap is scanned and forwarded");
    assert_eq!(
        engine.redact_calls(),
        1,
        "that one really did reach a model"
    );
}

/// **The blocker this change exists for, end to end.** A context-heavy remote
/// send crosses the gate and reaches the wire.
///
/// 40 KiB is the shape of an ordinary context-budget-full turn:
/// `HarnessConfig::context_budget_bytes` is 32,768 and the system prompt, the
/// JSON envelope and the escaping ride on top of it. That is *over* one engine
/// window ([`REDACT_CHUNK_MAX_BYTES`], 27,070) and *under* the total cap, and
/// before chunked scanning it was refused: `ScanUnavailable`, fail-closed, and
/// every such turn dead with `[privacy] redact = true`.
///
/// Three assertions, and the third is what makes it chunking rather than a
/// bigger number: the send **forwards** the exact bytes, the verdict claims the
/// scan **ran**, and it cost **more than one** model call — one per window.
///
/// The twin underneath is the discrimination: the same size of payload with a
/// credential planted past the first window still blocks, so this is a scan
/// that looked at all of it rather than a gate that stopped looking.
#[tokio::test]
async fn a_context_budget_full_payload_is_scanned_across_windows_and_forwards() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let big = "the retry helper reads the manifest and writes one report line. ".repeat(640);
    assert!(
        big.len() > REDACT_CHUNK_MAX_BYTES && big.len() < REDACT_INPUT_MAX_BYTES,
        "the fixture must be over one engine window and under the total cap: {}",
        big.len()
    );
    let (request, provenance) = clean_provenance_payload(&big);
    let body = request.body.clone();

    egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a context-budget-full payload must reach the wire, not block");

    assert_eq!(
        capture.captured().len(),
        1,
        "the send must have reached the transport"
    );
    assert_eq!(
        capture.captured()[0].body,
        body,
        "and byte-for-byte: v1 detects, it never substitutes (BR-5, AC-9)"
    );
    let verdict = gate.only_verdict();
    assert_eq!(verdict.outcome(), Outcome::Clean);
    assert!(
        verdict.scanned(),
        "a forward on this path must be a scan that ran, not one that could not"
    );
    assert!(
        engine.redact_calls() > 1,
        "a payload larger than one engine window is scanned in several calls; \
         {} is what a raised cap would look like",
        engine.redact_calls()
    );

    // The discrimination: the same size, a credential past the first window,
    // and it still blocks — so the scan really did look at the whole thing.
    let planted = format!(
        "{} {} at the end.",
        "the retry helper reads the manifest and writes one report line. ".repeat(640),
        PATTERN_SENTINEL
    );
    let at = planted.find(PATTERN_SENTINEL).expect("fixture");
    assert!(
        at > REDACT_CHUNK_MAX_BYTES,
        "the planted credential must sit past the first window"
    );
    let (request, provenance) = clean_provenance_payload(&planted);
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("a credential past the first window must still block");
    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::Redaction { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        capture.captured().len(),
        1,
        "and nothing more reached the wire"
    );
}

// ===========================================================================
// REQ-586 AC-6 — the redact scan bounds a big route's byte budget, and a turn
// that fills the bounded budget is scanned rather than refused
// ===========================================================================

/// **REQ-586 AC-6 / BR-4.** On a machine with `[privacy] redact = true`, a
/// 128k provider's byte budget is [`REDACT_SCANNABLE_CONTEXT_BYTES`] — and a
/// turn that fills it reaches the wire, scanned across windows, rather than
/// dying as `ScanUnavailable`.
///
/// ## Why this is an egress test and not another arithmetic one
///
/// `harness::budget::derive` is pinned as arithmetic in its own module. What
/// that cannot show is the thing AC-6 is actually about: the bound and the cap
/// are two numbers in two modules, and "the budget is small enough" is only
/// true if a body assembled *at* the budget is a body the redactor will read.
/// So the budget is derived here by the production function and then spent —
/// the whole of it — through the real choke point, the real chunked scan and a
/// real engine. Nothing in this file restates 89,127.
///
/// ## The mutation this is TASK-192's target for
///
/// The second leg is `with_redact_scan(false)`'s answer for the same provider:
/// remove the router's redact-scan input (or `derive`'s clamp) and the route
/// carries [`BudgetBound::Window`], whose byte budget is 253,952 — **over**
/// [`REDACT_INPUT_MAX_BYTES`]. A turn that filled *that* budget is refused
/// unscanned, fail-closed, which is the REQ-562/REQ-577 collision this REQ
/// exists not to reintroduce. The two legs are one test because the claim is a
/// comparison: it is not that a big body scans, it is that the bound is what
/// makes it one the scanner can take.
///
/// ## Bytes per word
///
/// The filler is prose at ≈ 6.4 B/word (65-byte sentence, 10 words). The word
/// figure is incidental here — the redact scan is byte-denominated and it is
/// `budget_bytes` the bound moves (BR-4) — but it is stated so that a later
/// reader can see this fixture is not accidentally testing the word guard.
#[tokio::test]
async fn a_redact_scanned_128k_route_assembles_a_body_the_scan_reads_whole_and_forwards() {
    /// The provider window AC-6 names, and the reservation every route
    /// subtracts (`HarnessConfig::default().gen_params.max_tokens`).
    const WINDOW: u32 = 128_000;
    const RESERVATION: u32 = 1_024;

    let inputs = |redact_scan: bool| BudgetInputs {
        window: WINDOW,
        cap: 0,
        reservation: RESERVATION,
        is_local: false,
        redact_scan,
        provider_id: Some("frontier"),
    };
    let scanned = derive(inputs(true));
    let unscanned = derive(inputs(false));

    // The bound is the scan's, and it is the byte half it moved: the word
    // budget is the same window-derived number on both routes (BR-4).
    assert_eq!(scanned.bound, BudgetBound::RedactScan);
    assert_eq!(scanned.budget_bytes, REDACT_SCANNABLE_CONTEXT_BYTES);
    assert_eq!(unscanned.bound, BudgetBound::Window);
    assert_eq!(
        scanned.budget_tokens, unscanned.budget_tokens,
        "the scan bounds bytes; the words stay window-derived"
    );
    // And the comparison the second leg rests on, stated before it is spent.
    assert!(
        scanned.budget_bytes < REDACT_INPUT_MAX_BYTES
            && unscanned.budget_bytes > REDACT_INPUT_MAX_BYTES,
        "AC-6 is vacuous unless the unbounded budget is one the redactor \
         refuses: bounded {} vs unbounded {} against a {REDACT_INPUT_MAX_BYTES}-byte cap",
        scanned.budget_bytes,
        unscanned.budget_bytes
    );

    // **The wiring, not only the arithmetic** (added by TASK-192's sweep: the
    // mutation `budget_for` ignoring `with_redact_scan` left every assertion
    // above green, because both legs call `derive` themselves). What makes
    // `[privacy] redact = true` a *budget* is the router passing that flag
    // down, so the same two pairs are demanded of `Router::budget_for` — the
    // call the daemon actually makes. A router that drops the flag hands this
    // 128k route the 253,952-byte pair, which is the unscannable one leg 2
    // refuses.
    let router_for = |redact_scan: bool| {
        Router::new(CategoryTable::new(), Some("frontier".to_owned()))
            .with_provider(
                "frontier",
                ProviderKind::OpenaiCompatible,
                "claude-opus-4",
                CapabilityProfile {
                    max_context: WINDOW,
                    ..CapabilityProfile::default()
                },
                ProviderHealth::Healthy,
            )
            .with_redact_scan(redact_scan)
    };
    assert_eq!(
        router_for(true).budget_for(Some("frontier")),
        scanned,
        "the redact-scan flag must reach the derivation through the router, \
         not only through this test"
    );
    assert_eq!(
        router_for(false).budget_for(Some("frontier")),
        unscanned,
        "and with the scan off the same route is window-bound"
    );

    // --- Leg 1: a body filling the bounded budget is scanned and forwards. ---
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    // 65 bytes, 10 whitespace words — ≈ 6.4 B/word.
    const SENTENCE: &str = "the retry helper reads the manifest and writes one report line. ";
    let context = SENTENCE.repeat(scanned.budget_bytes / SENTENCE.len());
    assert!(
        context.len() > REDACT_CHUNK_MAX_BYTES,
        "a body at this bound spans several engine windows, or the chunking \
         leg below proves nothing: {}",
        context.len()
    );
    let (request, provenance) = clean_provenance_payload(&context);
    let body = request.body.clone();
    assert!(
        body.len() <= REDACT_INPUT_MAX_BYTES,
        "a body assembled at the bound must fit the cap it is derived from: \
         {} against {REDACT_INPUT_MAX_BYTES}",
        body.len()
    );

    egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a turn that fills the redact-bounded budget must reach the wire");

    assert_eq!(capture.captured().len(), 1);
    assert_eq!(
        capture.captured()[0].body,
        body,
        "byte-for-byte: v1 detects, it never substitutes"
    );
    let verdict = gate.only_verdict();
    assert_eq!(verdict.outcome(), Outcome::Clean);
    assert!(
        verdict.scanned(),
        "a forward on this path must be a scan that ran, not one that could not"
    );
    assert!(
        engine.redact_calls() > 1,
        "a body at the bound is several engine windows wide and is scanned in \
         several calls; {} would be a gate that stopped looking",
        engine.redact_calls()
    );

    // The discrimination: the same body with a credential planted past the
    // first window still blocks, so the scan really did read all of it.
    let planted = format!("{context} {PATTERN_SENTINEL} at the end.");
    let at = planted.find(PATTERN_SENTINEL).expect("fixture");
    assert!(
        at > REDACT_CHUNK_MAX_BYTES,
        "the planted credential must sit past the first window"
    );
    let (request, provenance) = clean_provenance_payload(&planted);
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("a credential past the first window must still block");
    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::Redaction { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(capture.captured().len(), 1, "and nothing more left");

    // --- Leg 2: the same route without the bound assembles an unscannable
    // body. This is the mutation `with_redact_scan` exists to prevent. ---
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let unbounded = SENTENCE.repeat(unscanned.budget_bytes / SENTENCE.len());
    let (request, provenance) = clean_provenance_payload(&unbounded);
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("a body filling the window-derived budget is unscannable");
    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::ScanUnavailable,
                ..
            }
        ),
        "the un-bounded budget must die as unscannable — that is the turn AC-6 \
         promises never happens because of its size: {err:?}"
    );
    assert!(capture.captured().is_empty());
    assert_eq!(
        engine.redact_calls(),
        0,
        "and it never reached a model: the cap short-circuits"
    );
}

// ===========================================================================
// AC-5 — the pin resolves to an engine-backed provider only
// ===========================================================================

/// **AC-5, asserted by captured bytes** (BUG-156, LESSON-485).
///
/// The configuration is the one BUG-156 was about: a **remote** provider
/// registered under the canonical local-tier id. `runtime::local_tier_id`
/// answers `None` for it — an HTTP endpoint cannot be proven to be on this
/// machine — so the pin has nothing to name and the scan is unavailable.
///
/// What makes this a test rather than a restatement is the gate's `remote_arm`:
/// this fixture's gate **can** build a `RemoteDuty` over the captured wire. So
/// "no scan-originated request was captured" is a fact about the resolution, not
/// about a gate that had no way to send one — and the second half proves it, by
/// re-running the identical fixture with the one derivation BUG-156 got wrong
/// and watching the scan land on the wire.
///
/// ## Why the assertion is a capture count and not an id comparison (AC-8d)
///
/// The discrimination leg below is exactly the state where an id comparison
/// *passes* — the provider serving the scan is called `local`, so
/// `assert_eq!(provider, "local")` holds — while a redaction prompt is sitting
/// in the captured wire. That is the whole of BUG-156 in one fixture, and it is
/// why the AC says "by captured bytes, not by an id comparison". The mutation is
/// asserted here rather than described, so the weaker assertion's blindness is
/// part of the suite.
#[tokio::test]
async fn a_remote_provider_squatting_the_local_id_never_receives_the_scan() {
    /// A gate arm that routes the scan **over HTTP** to `provider_id`.
    fn remote_arm(capture: &CaptureTransport) -> RemoteArm {
        let capture = capture.clone();
        Box::new(move |provider_id: &str| {
            DutyRoute::remote(
                REDACT_DUTY,
                provider_id,
                Box::new(WireProvider),
                Egress::new(capture.clone(), Vec::new(), Arc::new(NoopSink)),
                "squatter-model",
                "sess-redacted",
                ResolvedEffort::effort(EffortLevel::High),
            )
        })
    }

    // --- The claim: `local_tier_id` yields nothing, so the pin names nothing.
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let gate = Arc::new(
        // What the daemon derives for a config whose only `local` entry is a
        // remote provider: no local tier (BUG-156/TASK-057).
        TestGate::new(router_with_local_tier(None)).with_remote_arm(remote_arm(&capture)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) = clean_provenance_payload("ordinary prose");
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("no tier can serve the scan, so the payload fails closed");
    assert!(
        matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::ScanUnavailable,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        capture.scan_originated(),
        0,
        "AC-5: the scan must never dispatch over HTTP"
    );
    assert!(
        capture.captured().is_empty(),
        "and the blocked turn sent nothing either"
    );

    // --- The discrimination: the same fixture with the derivation BUG-156 got
    //     wrong. The squatter *is* named, the arm builds a remote duty, and the
    //     redaction prompt lands on the wire. If this leg captured nothing, the
    //     leg above would be proving that a scan cannot be sent at all.
    let leaky_capture = CaptureTransport::default();
    let leaky_sink = Arc::new(CapturingSink::default());
    let leaky_gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .with_remote_arm(remote_arm(&leaky_capture)),
    );
    let leaky = choke_point(&leaky_capture, &leaky_sink, Arc::clone(&leaky_gate));
    let (request, provenance) = clean_provenance_payload("ordinary prose");
    let _ = leaky.send(request, &provenance, &ctx()).await;

    assert_eq!(
        leaky_capture.scan_originated(),
        1,
        "the wire under this fixture genuinely records a dispatched scan, so the \
         zero above is a fact about the resolution rather than about the wire"
    );
    // AC-8(d), asserted rather than described: the id comparison the AC forbids
    // is TRUE in exactly the state where the scan just went out over HTTP.
    assert_eq!(
        leaky_gate.redact_route().provider(),
        Some(CANONICAL_LOCAL_ID),
        "an id-based locality assertion passes here — which is why it is not the \
         assertion this AC uses"
    );
}

// ===========================================================================
// AC-4 — no locality guard was added, asserted behaviourally
// ===========================================================================

/// **AC-4's "no locality guard was added" leg** (BR-2, LESSON-484, LESSON-443).
///
/// A `[[providers]]` entry declaring `kind = "local"` under the id `on-device`
/// is a perfectly ordinary config, and it is a **genuinely engine-backed** local
/// tier: `runtime::local_tier_id` returns that id, and the pin resolves to it.
///
/// The scan runs and serves. An id-comparison guard anywhere on this path —
/// `if provider_id != "local" { unavailable }` — would refuse this fixture and
/// fail closed on a machine that has a perfectly good local tier, which is why
/// its *success* is the discriminating evidence that no such guard exists
/// (LESSON-485). Asserted behaviourally rather than by grepping `src/` for a
/// comparison, because a source grep tests the spelling and not the behaviour
/// (LESSON-489/BUG-159).
///
/// The pin is still the pin: the same router binds a remote `frontier` as the
/// default provider, and the scan does not go there.
#[tokio::test]
async fn an_engine_backed_local_tier_under_another_id_still_serves_the_scan() {
    const NON_CANONICAL: &str = "on-device";
    assert_ne!(
        NON_CANONICAL, CANONICAL_LOCAL_ID,
        "the fixture's whole point is an id that is not the canonical one"
    );

    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let engine = engine_pair("NONE");
    let router = router_with_local_tier(Some(NON_CANONICAL));
    assert_eq!(
        router.default_provider(),
        Some("frontier"),
        "a remote default is configured, so 'it went local' is a real choice"
    );
    let gate = Arc::new(TestGate::new(router).serving(Arc::clone(&engine.engine)));
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    // The pin resolves to the engine-backed tier under its own name.
    assert_eq!(gate.redact_route().provider(), Some(NON_CANONICAL));

    let (request, provenance) = clean_provenance_payload("a perfectly ordinary payload");
    egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a local tier under a non-canonical id still serves the scan");

    let verdict = gate.only_verdict();
    assert_eq!(
        verdict.outcome(),
        Outcome::Clean,
        "the scan served — an id comparison here would have made it Unavailable"
    );
    assert!(verdict.scanned());
    assert_eq!(engine.redact_calls(), 1, "on this machine's own engine");
    assert_eq!(
        capture.scan_originated(),
        0,
        "and it did not leave the machine to do it"
    );

    // The same gate still fails closed where it should: strip the tier and the
    // very next payload is blocked. Without this the test above would pass
    // against a gate that served everything unconditionally.
    let bare = Arc::new(TestGate::new(router_with_local_tier(None)));
    let bare_egress = choke_point(&capture, &sink, Arc::clone(&bare));
    let (request, provenance) = clean_provenance_payload("a perfectly ordinary payload");
    bare_egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("no tier, no scan, no send");
}

// ===========================================================================
// AC-12 — session taint short-circuits ahead of all of this
// ===========================================================================

/// **AC-12, by scanner call count** (BR-8).
///
/// A tainted session is pinned to the local tier, and the local tier has no
/// egress: `DutyRoute::local` holds an engine handle and no transport, so a turn
/// served there crosses no choke point and therefore no gate. The redactor is a
/// second line for content that was going to leave; it is not a substitute for
/// the pin that stops content leaving.
///
/// Asserted as the pair that makes the count mean something: the **same** gate,
/// the **same** payload, over a local route (zero scans) and over a remote route
/// (one scan). Without the second leg, "zero" is equally consistent with a gate
/// that was never installed.
///
/// The resolver half — that a tainted session resolves local in the first place
/// — is pinned where the resolver lives
/// (`runtime::tests::dispatch::redact::a_tainted_session_resolves_redact_exactly_as_a_clean_one_does`)
/// and end to end in `tests/e2e/privacy_fixes.rs`.
#[tokio::test]
async fn a_turn_pinned_to_the_local_tier_costs_zero_scanner_calls() {
    let capture = CaptureTransport::default();
    let engine = engine_pair("NONE");
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );

    let prompt = format!("summarize this for me: {PATTERN_SENTINEL}");

    // The tainted session's shape: served on the local tier, which holds no
    // transport at all.
    let local = DutyRoute::local(REDACT_DUTY, CANONICAL_LOCAL_ID, Arc::clone(&engine.engine));
    local
        .perform(&prompt, &Provenance::empty())
        .await
        .expect("the local tier serves it");
    assert_eq!(
        gate.calls(),
        0,
        "a turn that never reaches remote egress never reaches the gate"
    );
    assert!(
        capture.captured().is_empty(),
        "and nothing was sent, which is what the taint pin is for"
    );

    // The pair: the same content on a route that *does* cross the choke point.
    let remote = DutyRoute::remote(
        REDACT_DUTY,
        "frontier",
        Box::new(WireProvider),
        choke_point(
            &capture,
            &Arc::new(CapturingSink::default()),
            Arc::clone(&gate),
        ),
        "claude-opus-4",
        "sess-redacted",
        ResolvedEffort::effort(EffortLevel::High),
    );
    let _ = remote.perform(&prompt, &Provenance::empty()).await;
    assert_eq!(
        gate.calls(),
        1,
        "every remote path crosses the gate, so the zero above is a fact about \
         the local path rather than about an uninstalled gate"
    );
}

// ===========================================================================
// AC-6 — the sentinel sweep
// ===========================================================================

/// **AC-6.** Plant a distinctive sentinel, get it caught, then sweep **every
/// surface the run emitted** for it: the captured wire, the serialized
/// `privacy_block`, the typed error's `Display` and `Debug`, the transport error
/// the adapter seam sees, and the verdict the gate handed back.
///
/// The sweep is run with the model *quoting the sentinel back*, which is the
/// hardest case: the reply that comes into `read_findings` contains the secret
/// verbatim. ADR-5's quarantine is what keeps it there — spans leave, text does
/// not — and `Finding` has no text field for it to survive in.
///
/// This should come back clean by construction rather than by care. A hit means
/// somebody added a field, which is AC-8's mutation (c).
#[tokio::test]
async fn no_emitted_surface_carries_the_sentinel() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    // The model quotes the sentinel verbatim — the reply is the surface most
    // likely to leak it, and the one ADR-5 quarantines.
    let engine = engine_pair(&model_reports("credential", PATTERN_SENTINEL));
    let gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&engine.engine)),
    );
    let egress = choke_point(&capture, &sink, Arc::clone(&gate));

    let (request, provenance) =
        clean_provenance_payload(&format!("AWS_API_KEY={PATTERN_SENTINEL}\n"));
    let err = egress
        .send(request, &provenance, &ctx())
        .await
        .expect_err("the sentinel is pattern-shaped, so it blocks");

    // The surfaces, collected the way a subscriber, a log, and an adapter would
    // each see them.
    let block = sink.only();
    let mut surfaces: Vec<(&str, String)> = vec![
        ("typed error Display", err.to_string()),
        ("typed error Debug", format!("{err:?}")),
        ("privacy_block Debug", format!("{block:?}")),
        (
            "privacy_block wire form",
            serde_json::to_string(&Event::PrivacyBlock(block.clone())).expect("serialize"),
        ),
        ("verdict Debug", format!("{:?}", gate.only_verdict())),
        (
            "transport seam",
            format!("{:?}", err.clone().into_transport_error()),
        ),
    ];
    for (i, body) in capture.bodies().into_iter().enumerate() {
        surfaces.push(("captured wire", format!("#{i}: {body}")));
    }

    for (name, text) in &surfaces {
        assert!(
            !text.contains(PATTERN_SENTINEL),
            "BR-6 VIOLATION — the matched text reached the {name}: {text}"
        );
        // The sentinel's distinctive middle, in case someone renders a slice of
        // it rather than the whole thing.
        assert!(
            !text.contains("SENTINEL562"),
            "BR-6 VIOLATION — part of the matched text reached the {name}: {text}"
        );
    }

    // Non-vacuity: the sweep is looking at surfaces that genuinely exist and
    // genuinely describe this block. A sweep over empty strings passes too.
    assert!(
        surfaces
            .iter()
            .any(|(_, text)| text.contains("credential") || text.contains("Credential")),
        "the surfaces must actually report the finding's kind: {surfaces:?}"
    );
    assert!(
        surfaces
            .iter()
            .any(|(_, text)| text.contains("the outbound payload")),
        "and its non-secret locus: {surfaces:?}"
    );
    assert_eq!(
        engine.redact_calls(),
        1,
        "the model really was asked, so its reply really did carry the sentinel"
    );

    // -----------------------------------------------------------------------
    // The forwarding leg — the surface a *blocked* payload never reaches.
    // -----------------------------------------------------------------------
    //
    // A Low-only verdict forwards, and its findings are reported to the daemon
    // log instead of to `privacy_block`. That report is an emitted surface like
    // any other, and the sweep above cannot see it: on a blocked payload
    // `forwarded_findings_report` is empty by construction, so this leg needs a
    // payload that really does forward with a really located finding.
    //
    // The wire is deliberately NOT swept here. These bytes are being forwarded
    // on purpose — the sentinel is in them because the user's payload contains
    // it, which is BR-4's whole point. What must stay clean is what the daemon
    // *says* about them.
    let forward_capture = CaptureTransport::default();
    let forward_sink = Arc::new(CapturingSink::default());
    let forward_engine = engine_pair(&model_reports("credential", PARAPHRASED_SENTINEL));
    let forward_gate = Arc::new(
        TestGate::new(router_with_local_tier(Some(CANONICAL_LOCAL_ID)))
            .serving(Arc::clone(&forward_engine.engine)),
    );
    let forward_egress = choke_point(&forward_capture, &forward_sink, Arc::clone(&forward_gate));
    let (request, provenance) = clean_provenance_payload(&format!(
        "Here is the runbook. Note that {PARAPHRASED_SENTINEL}, so be careful."
    ));
    forward_egress
        .send(request, &provenance, &ctx())
        .await
        .expect("a low-confidence finding forwards (BR-4)");

    let verdict = forward_gate.only_verdict();
    let report = tetond::egress::redact::forwarded_findings_report(&verdict);
    // Non-vacuity: there really is a report line, and it really describes a
    // finding located in the sentinel-bearing payload.
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(report[0].contains("credential"), "{}", report[0]);

    let mut forward_surfaces: Vec<(&str, String)> = vec![
        ("verdict Debug", format!("{verdict:?}")),
        ("forwarded-findings report", report.join("\n")),
    ];
    forward_surfaces.push((
        "privacy_block events",
        format!("{:?}", forward_sink.events()),
    ));
    for (name, text) in &forward_surfaces {
        assert!(
            !text.contains(PARAPHRASED_SENTINEL) && !text.contains("orange-walrus"),
            "BR-6 VIOLATION — the matched text reached the {name}: {text}"
        );
    }
    assert_eq!(
        forward_engine.redact_calls(),
        1,
        "the model really was asked here too, so its reply carried the sentinel"
    );
}
