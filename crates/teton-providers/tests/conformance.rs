//! Shared conformance suite for the provider adapters (AC-1..AC-5).
//!
//! Both adapters are driven through the *same* assertions against a mock
//! [`Transport`] fed recorded streaming fixtures (Anthropic SSE and OpenAI
//! chat/completions chunks). The mock deliberately re-chunks each fixture at a
//! small, awkward byte size so the SSE framer's cross-boundary buffering is
//! exercised on every run. The suite asserts the load-bearing behaviors:
//! streaming deltas arrive in order, tool-call fragments assemble into one
//! parsed call, token usage is populated on every completed turn (BR-2),
//! malformed tool-call JSON is classified and surfaced without panicking, and
//! failure statuses/timeouts map to the right retry/fallback decisions.

use async_trait::async_trait;
use futures::executor::block_on;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use teton_providers::{
    AnthropicAdapter, EffortLevel, EffortOmission, FailureAction, Message, OpenAiCompatAdapter,
    OpenAiCompatConfig, Provider, ProviderError, ResolvedEffort, Role, StopReason, ToolCall,
    ToolSpec, Transport, TransportError, TransportRequest, TransportResponse, TurnEvent,
    TurnRequest,
};

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

/// A `Transport` that replays fixed bytes; it never touches the network. This is
/// the only kind of transport the adapters ever see in tests, which is exactly
/// the D-2 guarantee: no adapter can reach out on its own.
struct MockTransport {
    status: u16,
    chunks: Vec<Vec<u8>>,
    open_error: Option<TransportError>,
}

impl MockTransport {
    fn ok(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            chunks,
            open_error: None,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            chunks: Vec::new(),
            open_error: None,
        }
    }

    fn open_error(err: TransportError) -> Self {
        Self {
            status: 0,
            chunks: Vec::new(),
            open_error: Some(err),
        }
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        if let Some(err) = self.open_error {
            return Err(err);
        }
        let chunks = self.chunks.clone();
        let body = futures::stream::iter(chunks.into_iter().map(Ok::<Vec<u8>, TransportError>));
        Ok(TransportResponse {
            location: None,
            status: self.status,
            body: Box::pin(body),
        })
    }
}

/// Split a fixture into small byte chunks so cross-boundary buffering is tested.
fn chunkify(fixture: &str, size: usize) -> Vec<Vec<u8>> {
    fixture
        .as_bytes()
        .chunks(size)
        .map(<[u8]>::to_vec)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared drivers
// ---------------------------------------------------------------------------

fn sample_request() -> TurnRequest {
    // The pre-REQ-559 baseline: no reasoning field on the wire, so every
    // existing assertion in this suite keeps describing the same bytes.
    request_with(ResolvedEffort::omit(EffortOmission::ShapeNone))
}

fn request_with(effort: ResolvedEffort) -> TurnRequest {
    TurnRequest {
        model: "test-model".to_string(),
        system: Some("be helpful".to_string()),
        messages: vec![Message {
            role: Role::User,
            content: "weather in Paris?".to_string(),
        }],
        tools: vec![ToolSpec {
            name: "get_weather".to_string(),
            description: "look up the weather".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_tokens: 256,
        effort,
    }
}

// ---------------------------------------------------------------------------
// REQ-559: request-body capture
// ---------------------------------------------------------------------------

/// A `Transport` that records every request body it is handed before replaying
/// a canned success. AC-1 and AC-2 are claims about what leaves the adapter, and
/// only a capture can discharge them — reading the body-builder source is
/// exactly the "code inspection is not acceptance" that conventions.md rules out
/// for wire-level claims.
#[derive(Default)]
struct CapturingTransport {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl CapturingTransport {
    fn bodies(&self) -> Vec<serde_json::Value> {
        self.seen.lock().expect("capture lock").clone()
    }
}

#[async_trait]
impl Transport for CapturingTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.seen
            .lock()
            .expect("capture lock")
            .push(serde_json::from_slice(&request.body).expect("adapters emit JSON bodies"));
        // Enough of a stream to reach `Completed` on either adapter; the tests
        // below assert on the captured request, not on the response.
        let body = futures::stream::iter(std::iter::empty::<Result<Vec<u8>, TransportError>>());
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(body),
        })
    }
}

/// Drive both adapters once with `effort` and return the captured bodies,
/// labelled by adapter. The response is ignored — these are request-shape tests.
fn capture_both(effort: ResolvedEffort) -> Vec<(&'static str, serde_json::Value)> {
    let mut out = Vec::new();
    for (name, adapter) in [
        (
            "anthropic",
            Box::new(AnthropicAdapter::new("a", "https://api.anthropic.test")) as Box<dyn Provider>,
        ),
        (
            "openai-compatible",
            Box::new(OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
                "o",
                "https://api.openai.test",
            ))) as Box<dyn Provider>,
        ),
    ] {
        let transport = CapturingTransport::default();
        // The stream is allowed to fail (an empty body is a truncated stream);
        // the request has already been built and captured by then.
        let _ = block_on(adapter.stream_turn(request_with(effort), &transport));
        for body in transport.bodies() {
            out.push((name, body));
        }
    }
    assert_eq!(out.len(), 2, "both adapters must have issued one request");
    out
}

/// The two keys that must never appear together in one body (BR-4).
fn reasoning_keys(body: &serde_json::Value) -> (bool, bool) {
    let effort_field =
        body.get("reasoning_effort").is_some() || body.pointer("/output_config/effort").is_some();
    let thinking_field = body.get("thinking").is_some();
    (effort_field, thinking_field)
}

/// AC-1 / BR-1. Every outbound request to a provider resolving to `Effort`
/// carries an effort field. Driven across all five canonical levels and both
/// adapters, so no call path can omit it.
///
/// The compiler already guarantees a *value* is supplied (`TurnRequest.effort`
/// is required and `ResolvedEffort` has no `Default` — ADR-B). This asserts the
/// value is honest: that it actually reaches the wire.
#[test]
fn every_effort_resolution_puts_the_field_on_the_wire() {
    for level in [
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Xhigh,
        EffortLevel::Max,
    ] {
        for (adapter, body) in capture_both(ResolvedEffort::effort(level)) {
            let (has_effort, _) = reasoning_keys(&body);
            assert!(
                has_effort,
                "{adapter} dropped the effort field at {level}; omission inherits \
                 the provider's default, and Kimi K3's is `max` (BR-1)",
            );
            let sent = body
                .get("reasoning_effort")
                .or_else(|| body.pointer("/output_config/effort"))
                .and_then(serde_json::Value::as_str)
                .expect("the effort field is a string");
            assert_eq!(
                sent,
                level.as_str(),
                "{adapter} must send the canonical spelling"
            );
        }
    }
}

/// AC-2 / BR-4. No request ever carries both shapes. Kimi K2.5/K2.6 answer HTTP
/// 400 when both are sent, so this is a correctness constraint.
///
/// ADR-A makes it unrepresentable — no `ResolvedEffort` variant names two fields
/// — and this test drives every variant through both adapters to prove the
/// property holds on the wire and not merely in the type.
#[test]
fn no_request_ever_carries_both_reasoning_shapes() {
    let variants = [
        ResolvedEffort::effort(EffortLevel::High),
        ResolvedEffort::ThinkingFlag,
        ResolvedEffort::omit(EffortOmission::ShapeNone),
        ResolvedEffort::omit(EffortOmission::EmptyLadder),
        ResolvedEffort::omit(EffortOmission::RefusedThisSession),
    ];
    for effort in variants {
        for (adapter, body) in capture_both(effort) {
            let (has_effort, has_thinking) = reasoning_keys(&body);
            assert!(
                !(has_effort && has_thinking),
                "{adapter} sent both shapes for {effort:?} — a 400 on Kimi K2.5/K2.6",
            );
            match effort {
                ResolvedEffort::Effort { .. } => assert!(has_effort && !has_thinking),
                ResolvedEffort::ThinkingFlag => assert!(has_thinking && !has_effort),
                ResolvedEffort::Omit { .. } => assert!(
                    !has_effort && !has_thinking,
                    "{adapter} sent a reasoning field for an omitted resolution",
                ),
            }
        }
    }
}

/// ADR-H, pinned so a future reader does not "fix" it. Anthropic accepts
/// `output_config.effort` and `thinking` together; we deliberately send only the
/// former, because BR-4 makes single-shape a testable invariant that holds for
/// every provider, and Anthropic's thinking is already adaptive when effort is
/// set. The omission is a decision, not an oversight.
#[test]
fn anthropic_sends_effort_alone_even_though_it_accepts_both() {
    let transport = CapturingTransport::default();
    let adapter = AnthropicAdapter::new("a", "https://api.anthropic.test");
    let _ = block_on(adapter.stream_turn(
        request_with(ResolvedEffort::effort(EffortLevel::Xhigh)),
        &transport,
    ));
    let body = &transport.bodies()[0];
    assert_eq!(
        body.pointer("/output_config/effort")
            .and_then(serde_json::Value::as_str),
        Some("xhigh"),
    );
    assert!(
        body.get("thinking").is_none(),
        "ADR-H: no thinking block on the effort_only shape",
    );
}

/// BR-6 compatibility: under `Omit` the bodies are byte-identical to the
/// pre-REQ-559 bodies. The addition is inert when effort does not apply, which
/// is what lets the local tier and every existing test keep their meaning.
#[test]
fn an_omitted_resolution_leaves_the_body_unchanged() {
    for reason in [
        EffortOmission::ShapeNone,
        EffortOmission::EmptyLadder,
        EffortOmission::RefusedThisSession,
    ] {
        let omitted = capture_both(ResolvedEffort::omit(reason));
        let baseline = capture_both(ResolvedEffort::omit(EffortOmission::ShapeNone));
        assert_eq!(omitted, baseline, "{reason:?} must not alter the wire");
        for (adapter, body) in &omitted {
            let obj = body.as_object().expect("a JSON object body");
            assert!(
                !obj.contains_key("reasoning_effort")
                    && !obj.contains_key("output_config")
                    && !obj.contains_key("thinking"),
                "{adapter} added a reasoning key for an omitted resolution",
            );
        }
    }
}

/// Drive an adapter to completion, bubbling the first error (open or mid-stream).
fn run(adapter: &dyn Provider, transport: &MockTransport) -> Result<Vec<TurnEvent>, ProviderError> {
    block_on(async {
        let mut stream = adapter.stream_turn(sample_request(), transport).await?;
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item?);
        }
        Ok(events)
    })
}

/// The assertions every conforming adapter must satisfy on the happy path.
fn assert_conformant_turn(events: &[TurnEvent]) {
    // Streaming text deltas arrive in order and reassemble.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world", "text deltas must stream in order");

    // Tool-call fragments assemble into exactly one parsed call.
    let tools: Vec<&ToolCall> = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1, "one tool call expected");
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(tools[0].arguments, serde_json::json!({"city": "Paris"}));
    assert!(!tools[0].id.is_empty(), "tool call id must be captured");

    // Exactly one terminal Completed, and it is last, with usage populated (BR-2).
    let completed_count = events
        .iter()
        .filter(|e| matches!(e, TurnEvent::Completed(_)))
        .count();
    assert_eq!(completed_count, 1, "exactly one Completed event");
    match events.last() {
        Some(TurnEvent::Completed(c)) => {
            assert!(c.usage.input_tokens > 0, "input tokens must be populated");
            assert!(c.usage.output_tokens > 0, "output tokens must be populated");
            assert_eq!(c.stop_reason, StopReason::ToolUse);
        }
        other => panic!("expected Completed last, got {other:?}"),
    }

    // Ordering contract: all text deltas precede the tool call, which precedes
    // Completed.
    let idx_last_text = events
        .iter()
        .rposition(|e| matches!(e, TurnEvent::TextDelta(_)))
        .unwrap();
    let idx_tool = events
        .iter()
        .position(|e| matches!(e, TurnEvent::ToolCall(_)))
        .unwrap();
    let idx_completed = events
        .iter()
        .position(|e| matches!(e, TurnEvent::Completed(_)))
        .unwrap();
    assert!(idx_last_text < idx_tool, "text precedes tool call");
    assert!(idx_tool < idx_completed, "tool call precedes completion");
}

fn anthropic() -> AnthropicAdapter {
    AnthropicAdapter::new("anthropic", "https://example.test/v1/messages")
}

fn openai() -> OpenAiCompatAdapter {
    OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://example.test/v1/chat/completions",
    ))
}

// ---------------------------------------------------------------------------
// Fixtures — recorded streaming responses.
// ---------------------------------------------------------------------------

const ANTHROPIC_HAPPY: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"Paris\\\"}\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":1}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":17}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

/// Same as the happy path but the tool's argument fragments never close the
/// JSON object.
const ANTHROPIC_MALFORMED_TOOL: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_stop
data: {\"type\":\"message_stop\"}

";

const OPENAI_HAPPY: &str = "\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\" \\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":17}}

data: [DONE]

";

const OPENAI_MALFORMED_TOOL: &str = "\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":17}}

data: [DONE]

";

// ---------------------------------------------------------------------------
// Conformance tests
// ---------------------------------------------------------------------------

/// One conformance case: an adapter (as a trait object) and its fixture chunks.
type Case = (Box<dyn Provider>, Vec<Vec<u8>>);

#[test]
fn both_adapters_pass_the_shared_conformance_suite() {
    // The same assertions, run over both adapters as trait objects (also proving
    // `Provider` is object-safe).
    let cases: Vec<Case> = vec![
        (Box::new(anthropic()), chunkify(ANTHROPIC_HAPPY, 7)),
        (Box::new(openai()), chunkify(OPENAI_HAPPY, 7)),
    ];
    for (adapter, chunks) in cases {
        let transport = MockTransport::ok(chunks);
        let events = run(adapter.as_ref(), &transport)
            .unwrap_or_else(|e| panic!("adapter {} should complete: {e}", adapter.id()));
        assert_conformant_turn(&events);
    }
}

#[test]
fn anthropic_malformed_tool_call_is_classified_never_panics() {
    let transport = MockTransport::ok(chunkify(ANTHROPIC_MALFORMED_TOOL, 9));
    let err = run(&anthropic(), &transport).expect_err("malformed tool JSON must surface an error");
    assert!(
        matches!(err, ProviderError::MalformedToolCall { .. }),
        "got {err:?}"
    );
    assert_eq!(
        err.decision().map(|d| d.action),
        Some(FailureAction::Degrade)
    );
}

#[test]
fn openai_malformed_tool_call_is_classified_never_panics() {
    let transport = MockTransport::ok(chunkify(OPENAI_MALFORMED_TOOL, 9));
    let err = run(&openai(), &transport).expect_err("malformed tool JSON must surface an error");
    assert!(
        matches!(err, ProviderError::MalformedToolCall { .. }),
        "got {err:?}"
    );
    assert_eq!(
        err.decision().map(|d| d.action),
        Some(FailureAction::Degrade)
    );
}

#[test]
fn client_error_status_maps_to_fallback() {
    let err = run(&anthropic(), &MockTransport::status(404))
        .expect_err("4xx should surface before any events");
    assert!(matches!(err, ProviderError::ClientError { status: 404 }));
    assert_eq!(
        err.decision().map(|d| d.action),
        Some(FailureAction::Fallback)
    );
}

#[test]
fn auth_error_status_maps_to_fail() {
    let err = run(&openai(), &MockTransport::status(401)).expect_err("401 should surface");
    assert!(matches!(err, ProviderError::ClientError { status: 401 }));
    assert_eq!(err.decision().map(|d| d.action), Some(FailureAction::Fail));
}

#[test]
fn server_error_status_maps_to_retry() {
    let err = run(&openai(), &MockTransport::status(503)).expect_err("5xx should surface");
    assert!(matches!(err, ProviderError::ServerError { status: 503 }));
    let decision = err.decision().expect("server error is classified");
    assert_eq!(decision.action, FailureAction::Retry);
    assert!(decision.retryable);
}

#[test]
fn open_timeout_maps_to_retry() {
    let err = run(
        &anthropic(),
        &MockTransport::open_error(TransportError::Timeout),
    )
    .expect_err("timeout should surface");
    assert_eq!(err, ProviderError::Timeout);
    assert_eq!(err.decision().map(|d| d.action), Some(FailureAction::Retry));
}

#[test]
fn mid_stream_transport_error_is_surfaced() {
    // A body chunk that errors mid-stream (not an open error) surfaces as a
    // yielded Err without panicking.
    struct MidError;
    #[async_trait]
    impl Transport for MidError {
        async fn execute(
            &self,
            _request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            let items: Vec<Result<Vec<u8>, TransportError>> = vec![
                Ok(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n".to_vec()),
                Err(TransportError::Io),
            ];
            Ok(TransportResponse {
                location: None,
                status: 200,
                body: Box::pin(futures::stream::iter(items)),
            })
        }
    }

    let err = run(&anthropic(), &MockTransport::ok(vec![]));
    assert!(err.is_ok(), "empty stream still finalizes");

    let result = block_on(async {
        let mut stream = anthropic()
            .stream_turn(sample_request(), &MidError)
            .await
            .expect("stream opens");
        let mut last = None;
        while let Some(item) = stream.next().await {
            last = Some(item);
        }
        last
    });
    assert_eq!(result, Some(Err(ProviderError::Transport)));
}

#[test]
fn empty_body_still_finalizes_with_usage_zeroed() {
    // Degenerate case: a provider that returns nothing still yields a single
    // Completed (never leaves the turn hanging).
    let events = run(&anthropic(), &MockTransport::ok(vec![])).expect("finalizes");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], TurnEvent::Completed(_)));
}

/// REQ-559 BR-10: the Anthropic API reports no reasoning-token count, so its
/// `TokenUsage.reasoning_tokens` is `None` — unreported, which is the truth
/// about it. Pinned so a future reader does not read the absence as a gap in
/// this REQ's parsing and "fix" it by writing a zero.
#[test]
fn anthropic_reports_no_reasoning_split() {
    let fixture = concat!(
        "event: message_start\n",
        "data: {\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {}\n\n",
    );
    let transport = MockTransport::ok(chunkify(fixture, 7));
    let adapter = AnthropicAdapter::new("a", "https://api.anthropic.test");
    let events = run(&adapter, &transport).expect("stream");
    let TurnEvent::Completed(done) = events.last().expect("a terminal event") else {
        panic!("the last event must be Completed");
    };
    assert_eq!(done.usage.output_tokens, 7);
    assert_eq!(
        done.usage.reasoning_tokens, None,
        "Anthropic reports no split; None is unreported, not zero",
    );
}

/// AC-9 at the adapter seam: an OpenAI-compatible usage chunk carrying
/// `completion_tokens_details.reasoning_tokens` yields that value, and
/// `output_tokens` is the same number the parser produced before this field
/// existed — the subset relationship, proven rather than asserted in prose.
#[test]
fn openai_parses_the_reasoning_split_without_moving_the_total() {
    fn usage_of(fixture: &str) -> teton_providers::TokenUsage {
        let transport = MockTransport::ok(chunkify(fixture, 9));
        let adapter =
            OpenAiCompatAdapter::new(OpenAiCompatConfig::new("o", "https://api.openai.test"));
        let events = run(&adapter, &transport).expect("stream");
        match events.last().expect("a terminal event") {
            TurnEvent::Completed(done) => done.usage,
            other => panic!("the last event must be Completed, got {other:?}"),
        }
    }

    let with_split = usage_of(concat!(
        "data: {\"choices\":[]}\n\n",
        "data: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":42,",
        "\"completion_tokens_details\":{\"reasoning_tokens\":30}}}\n\n",
        "data: [DONE]\n\n",
    ));
    let without = usage_of(concat!(
        "data: {\"choices\":[]}\n\n",
        "data: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":42}}\n\n",
        "data: [DONE]\n\n",
    ));

    assert_eq!(with_split.reasoning_tokens, Some(30));
    assert_eq!(without.reasoning_tokens, None, "unreported, never 0");
    assert_eq!(
        with_split.output_tokens, without.output_tokens,
        "BR-10: parsing the split must not move the total",
    );
    assert_eq!(with_split.output_tokens, 42);
    assert!(with_split.reasoning_tokens.unwrap() <= with_split.output_tokens);
}
