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

/// The kimi-k3 turn shape (BUG-178): `reasoning_content` deltas, then a
/// `tool_calls` fragment, and every `content` delta the empty string — no
/// prose at all. Recorded from the failing session; the adapter must yield the
/// call and nothing text-shaped, which is exactly the turn the harness then has
/// to record without producing an empty assistant message.
const OPENAI_REASONING_ONLY_TOOL_CALL: &str = "\
data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"reasoning_content\":\"I should read \"},\"finish_reason\":null}]}

data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"reasoning_content\":\"the file.\"},\"finish_reason\":null}]}

data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: {\"id\":\"c2\",\"choices\":[],\"usage\":{\"prompt_tokens\":40,\"completion_tokens\":19,\"completion_tokens_details\":{\"reasoning_tokens\":12}}}

data: [DONE]

";

// ---------------------------------------------------------------------------
// Conformance tests
// ---------------------------------------------------------------------------

/// BUG-178 at the adapter: a reasoning-only native tool call is exactly one
/// `ToolCall` and **no** `TextDelta` — an empty `content` delta is not text,
/// and `reasoning_content` is never surfaced as text either. This is the
/// legitimate adapter output the harness's transcript then has to record as
/// the call rather than as an empty assistant turn.
#[test]
fn openai_a_reasoning_only_tool_call_yields_the_call_and_no_text() {
    let transport = MockTransport::ok(chunkify(OPENAI_REASONING_ONLY_TOOL_CALL, 11));
    let events = run(&openai(), &transport).expect("the turn completes");
    assert!(
        !events.iter().any(|e| matches!(e, TurnEvent::TextDelta(_))),
        "no text event for a turn with no prose: {events:?}"
    );
    let calls: Vec<&ToolCall> = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "{events:?}");
    assert_eq!(calls[0].id, "call_9");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, serde_json::json!({"city": "Paris"}));
    match events.last() {
        Some(TurnEvent::Completed(c)) => {
            assert_eq!(c.stop_reason, StopReason::ToolUse);
            assert_eq!(c.usage.output_tokens, 19);
            assert_eq!(c.usage.reasoning_tokens, Some(12));
        }
        other => panic!("expected Completed last, got {other:?}"),
    }
}

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

// ---------------------------------------------------------------------------
// REQ-559 BR-12: the effort refusal (AC-2b, AC-10)
// ---------------------------------------------------------------------------

/// A transport that answers one status with one body — enough to drive the 400
/// classification path, which reads a bounded prefix of the error document.
struct ErrorBodyTransport {
    status: u16,
    body: &'static str,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ErrorBodyTransport {
    fn new(status: u16, body: &'static str) -> Self {
        Self {
            status,
            body,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Transport for ErrorBodyTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.seen
            .lock()
            .expect("capture lock")
            .push(serde_json::from_slice(&request.body).expect("a JSON body"));
        let chunks = vec![Ok::<Vec<u8>, TransportError>(self.body.as_bytes().to_vec())];
        Ok(TransportResponse {
            location: None,
            status: self.status,
            body: Box::pin(futures::stream::iter(chunks)),
        })
    }
}

/// AC-2b / AC-10 / BR-12. A 400 naming the effort field produces the **typed**
/// error, and it names all three values BR-12 requires: the provider, the level
/// the user asked for, and the level the clamp actually sent.
///
/// The distinction from a generic `ClientError` is load-bearing: this error is
/// what populates the session refusal memo (ADR-F), and a memo poisoned by an
/// unrelated 400 would silently stop sending effort to a provider that accepts
/// it — the misattribution family BR-6 exists to prevent.
#[test]
fn a_400_naming_the_effort_field_is_the_typed_refusal() {
    let transport = ErrorBodyTransport::new(
        400,
        r#"{"error":{"message":"Unrecognized request argument supplied: reasoning_effort"}}"#,
    );
    let adapter = OpenAiCompatAdapter::new(OpenAiCompatConfig::new("mystery", "https://byom.test"));
    let err = match block_on(adapter.stream_turn(
        // Clamped: `xhigh` asked for, `high` sent — the pair the error must name.
        request_with(ResolvedEffort::clamped(
            EffortLevel::Xhigh,
            EffortLevel::High,
        )),
        &transport,
    )) {
        Ok(_) => panic!("a 400 must not open a stream"),
        Err(err) => err,
    };

    match &err {
        ProviderError::EffortRefused {
            provider_id,
            requested,
            clamped,
        } => {
            assert_eq!(provider_id, "mystery");
            assert_eq!(*requested, EffortLevel::Xhigh);
            assert_eq!(*clamped, EffortLevel::High);
        }
        other => panic!("expected the typed refusal, got {other:?}"),
    }
    // The message names all three, and carries no response body or prompt text.
    let msg = err.to_string();
    assert!(
        msg.contains("mystery") && msg.contains("xhigh") && msg.contains("high"),
        "{msg}"
    );
    assert!(
        !msg.contains("Unrecognized"),
        "no provider body in the error: {msg}"
    );

    // It is NOT handed to the retry/fallback/degrade machinery: the one correct
    // response is a single retry with no reasoning field, which the daemon does.
    assert!(err.is_effort_refused());
    assert_eq!(err.failure_class(), None);
}

/// The narrowness is the point. A 400 for an unrelated reason stays a generic
/// `ClientError`, so it cannot poison the session memo and silently disable
/// effort for a provider that accepts it.
#[test]
fn an_unrelated_400_is_not_read_as_an_effort_refusal() {
    let transport = ErrorBodyTransport::new(
        400,
        r#"{"error":{"message":"invalid model: no such model `test-model`"}}"#,
    );
    let adapter = OpenAiCompatAdapter::new(OpenAiCompatConfig::new("mystery", "https://byom.test"));
    let err = match block_on(adapter.stream_turn(
        request_with(ResolvedEffort::effort(EffortLevel::High)),
        &transport,
    )) {
        Ok(_) => panic!("a 400 must not open a stream"),
        Err(err) => err,
    };
    assert!(!err.is_effort_refused(), "got {err:?}");
    assert!(matches!(err, ProviderError::ClientError { status: 400 }));
}

/// A request that carried **no** reasoning field cannot be refusing one, so the
/// classification short-circuits. This is what makes the daemon's single retry
/// non-looping by construction rather than by a counter: the retry sends
/// `Omit`, and an `Omit` request can never come back as a refusal.
#[test]
fn a_request_with_no_reasoning_field_can_never_be_an_effort_refusal() {
    for reason in [
        EffortOmission::ShapeNone,
        EffortOmission::EmptyLadder,
        EffortOmission::RefusedThisSession,
    ] {
        let transport = ErrorBodyTransport::new(
            400,
            r#"{"error":{"message":"Unrecognized request argument supplied: reasoning_effort"}}"#,
        );
        let adapter =
            OpenAiCompatAdapter::new(OpenAiCompatConfig::new("mystery", "https://byom.test"));
        let err = match block_on(
            adapter.stream_turn(request_with(ResolvedEffort::omit(reason)), &transport),
        ) {
            Ok(_) => panic!("a 400 must not open a stream"),
            Err(err) => err,
        };
        assert!(
            !err.is_effort_refused(),
            "{reason:?}: a request that sent no reasoning field cannot be refusing one",
        );
    }
}

/// AC-2b's first leg: an `openai-compatible` provider with **no declared
/// reasoning_shape** sends the effort field on its first call — the ADR-E
/// `effort_only` default. Asserted on the captured body, because this is the
/// BYOM leg of AC-1's regression and the whole point is what leaves the daemon.
#[test]
fn an_undeclared_byom_endpoint_sends_the_effort_field_on_its_first_call() {
    let transport = ErrorBodyTransport::new(400, r#"{"error":"nope"}"#);
    let adapter = OpenAiCompatAdapter::new(OpenAiCompatConfig::new("mystery", "https://byom.test"));
    let _ = block_on(adapter.stream_turn(
        request_with(ResolvedEffort::effort(EffortLevel::High)),
        &transport,
    ));
    let bodies = transport.seen.lock().expect("capture lock").clone();
    assert_eq!(bodies.len(), 1, "exactly one request, no silent retry here");
    assert_eq!(bodies[0]["reasoning_effort"], "high");
    assert!(
        bodies[0].get("thinking").is_none(),
        "and never both shapes (AC-2b)",
    );
}

// ---------------------------------------------------------------------------
// REQ-586 BR-2 / ADR-8: the context-length refusal (AC-3)
// ---------------------------------------------------------------------------

/// Anthropic's 400 for a prompt that exceeds the window, in its documented
/// envelope ("prompt is too long" — verified 2026-08-19).
const ANTHROPIC_TOO_LONG_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213717 tokens > 200000 maximum"},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#;

/// OpenAI's 400 as it arrives on the wire — pretty-printed, `code:
/// "context_length_exceeded"`, and the "maximum context length" message
/// (verified 2026-08-19).
const OPENAI_TOO_LONG_BODY: &str = "{\n  \"error\": {\n    \"message\": \"This model's maximum context length is 128000 tokens. However, your messages resulted in 131000 tokens. Please reduce the length of the messages.\",\n    \"type\": \"invalid_request_error\",\n    \"param\": \"messages\",\n    \"code\": \"context_length_exceeded\"\n  }\n}";

/// A compact compat body carrying only the code, as a proxy or self-hosted
/// endpoint that reuses OpenAI's code but not its wording would send.
const COMPAT_CODE_ONLY_BODY: &str = r#"{"error":{"message":"Input too long for this endpoint","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;

/// Moonshot/Kimi's 400 for a prompt that exceeds the window, in the envelope
/// their error reference documents — `type` + `message`, no `code` at all
/// (verified 2026-08-19, <https://platform.kimi.ai/docs/api/errors>).
///
/// The case that matters most: Kimi is the dogfood provider, it is reached
/// through the OpenAI-compatible adapter, and its body carries **neither**
/// OpenAI spelling — so before this const it classified as a plain
/// `ClientError { 400 }` and cost the provider a health downgrade for a
/// request that was merely too big.
const MOONSHOT_TOO_LONG_BODY: &str =
    r#"{"error":{"type":"invalid_request_error","message":"Input token length too long"}}"#;

/// An unrelated 400 — neither vendor's spelling — which must keep today's path.
const UNRELATED_400_BODY: &str =
    r#"{"error":{"message":"invalid model: no such model `test-model`"}}"#;

fn drive_400(adapter: &dyn Provider, body: &'static str, effort: ResolvedEffort) -> ProviderError {
    let transport = ErrorBodyTransport::new(400, body);
    match block_on(adapter.stream_turn(request_with(effort), &transport)) {
        Ok(_) => panic!("a 400 must not open a stream"),
        Err(err) => err,
    }
}

/// AC-3 / BR-2. Each adapter maps its vendor's spelling to the **typed**,
/// class-less `ContextLengthExceeded` naming the provider — so the daemon can
/// end the turn with a report instead of a retry, a fallback, or a health
/// change. Driven on the `Omit` path (`sample_request`'s baseline) **and** on a
/// request that sent effort, because the effort sniff short-circuits on `Omit`
/// and ADR-8 says the head is read for every 400 regardless.
#[test]
fn each_adapter_maps_its_vendor_spelling_to_context_length_exceeded() {
    let cases: [(&str, &dyn Provider, &'static str); 4] = [
        ("anthropic", &anthropic(), ANTHROPIC_TOO_LONG_BODY),
        ("deepseek", &openai(), OPENAI_TOO_LONG_BODY),
        ("deepseek", &openai(), COMPAT_CODE_ONLY_BODY),
        // REQ-586 TASK-189: the dogfood provider's own spelling.
        ("deepseek", &openai(), MOONSHOT_TOO_LONG_BODY),
    ];
    for (expected_id, adapter, body) in cases {
        for effort in [
            ResolvedEffort::omit(EffortOmission::ShapeNone),
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
            ResolvedEffort::effort(EffortLevel::High),
        ] {
            let err = drive_400(adapter, body, effort);
            match &err {
                ProviderError::ContextLengthExceeded { provider_id } => {
                    assert_eq!(provider_id, expected_id, "{effort:?}");
                }
                other => panic!("expected the typed refusal, got {other:?} ({effort:?})"),
            }
            assert!(err.is_context_length_exceeded());
            assert!(!err.is_effort_refused());
            // Class-less: not handed to retry / fallback / degrade.
            assert_eq!(err.failure_class(), None, "{effort:?}");
            assert_eq!(err.decision(), None, "{effort:?}");
            // The message names the provider and carries no response body.
            let msg = err.to_string();
            assert!(msg.contains(expected_id), "{msg}");
            assert!(
                !msg.contains("213717") && !msg.contains("131000") && !msg.contains("too long"),
                "no provider body in the error: {msg}"
            );
        }
    }
}

/// The narrowness is the point. An unrelated 400 on either adapter is **still**
/// a generic `ClientError { 400 }` classified `Fallback` — the sniff moves no
/// existing outcome.
#[test]
fn an_unrelated_400_on_either_adapter_keeps_todays_fallback_path() {
    let adapters: [&dyn Provider; 2] = [&anthropic(), &openai()];
    for adapter in adapters {
        for effort in [
            ResolvedEffort::omit(EffortOmission::ShapeNone),
            ResolvedEffort::effort(EffortLevel::High),
        ] {
            let err = drive_400(adapter, UNRELATED_400_BODY, effort);
            assert!(
                matches!(err, ProviderError::ClientError { status: 400 }),
                "{}: {err:?} ({effort:?})",
                adapter.id()
            );
            assert!(!err.is_context_length_exceeded());
            assert_eq!(
                err.decision().map(|d| d.action),
                Some(FailureAction::Fallback),
                "{}: {effort:?}",
                adapter.id()
            );
        }
    }
}

/// The other vendor's spelling is still recognized by either adapter — the
/// classifier is shared (one implementation, lib.rs `classify_client_error`),
/// so a proxy that fronts Anthropic behind an OpenAI-shaped endpoint, or the
/// reverse, cannot slip a context-length refusal past the daemon as a generic
/// 400.
#[test]
fn the_context_length_classifier_is_shared_across_adapters() {
    let err = drive_400(
        &openai(),
        ANTHROPIC_TOO_LONG_BODY,
        ResolvedEffort::omit(EffortOmission::ShapeNone),
    );
    assert!(err.is_context_length_exceeded(), "{err:?}");
    let err = drive_400(
        &anthropic(),
        OPENAI_TOO_LONG_BODY,
        ResolvedEffort::omit(EffortOmission::ShapeNone),
    );
    assert!(err.is_context_length_exceeded(), "{err:?}");
}

/// Moonshot's *other* size refusal is a different failure and must keep the
/// generic path: `total message size N exceeds limit 2097152` is the 2 MB
/// request-body cap, not the token window, and compaction by word count does
/// not reliably fix it. Typing it as a context-length outcome would end the
/// turn with a budget report for a fault the budget cannot explain — the
/// narrowness ADR-8 asks for, on the one provider that sends both.
#[test]
fn moonshots_request_body_size_cap_is_not_a_context_length_refusal() {
    const BODY_SIZE_CAP: &str = r#"{"error":{"type":"invalid_request_error","message":"total message size 5943865 exceeds limit 2097152"}}"#;
    let err = drive_400(
        &openai(),
        BODY_SIZE_CAP,
        ResolvedEffort::omit(EffortOmission::ShapeNone),
    );
    assert!(!err.is_context_length_exceeded(), "{err:?}");
    assert!(
        matches!(err, ProviderError::ClientError { status: 400 }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// The endpoint contract (BUG-170)
// ---------------------------------------------------------------------------

/// A `Transport` that records the **URL** of every request it is handed.
///
/// [`CapturingTransport`] above records bodies, which is what the effort tests
/// needed. The claim below is about the other field, and it needs its own
/// capture for the same reason that one exists: reading `build_request`'s source
/// and concluding "it assigns `self.endpoint`" is code inspection, and a
/// wire-level claim is discharged by a capture or not at all.
#[derive(Default)]
struct UrlCapturingTransport {
    seen: Arc<Mutex<Vec<String>>>,
}

impl UrlCapturingTransport {
    fn urls(&self) -> Vec<String> {
        self.seen.lock().expect("capture lock").clone()
    }
}

#[async_trait]
impl Transport for UrlCapturingTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.seen.lock().expect("capture lock").push(request.url);
        let body = futures::stream::iter(std::iter::empty::<Result<Vec<u8>, TransportError>>());
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(body),
        })
    }
}

/// **A configured endpoint is the request URL, verbatim — no adapter joins a
/// path onto it** (BUG-170).
///
/// This is a load-bearing premise that nothing used to assert. The daemon's
/// recipe catalog ships one URL per vendor and
/// `provider_recipes::tests::every_recipe_is_a_registration_the_daemon_accepts_and_an_adapter_can_post`
/// checks each one's *path* against the adapter its kind selects — which is only
/// a meaningful check if the adapter really does request that exact path. Round
/// 1 of that catalog shipped five vendor `base_url`s precisely because everyone
/// involved assumed the opposite: that something downstream would complete the
/// URL the way an OpenAI SDK completes a `base_url`. Nothing does, and now
/// something says so.
///
/// The sentinel is deliberately **not** a plausible endpoint. A URL ending
/// `/chat/completions` would pass this test even under an adapter that appended
/// `/chat/completions` to whatever it was given; a sentinel with a nonsense path
/// and a query string can only survive being passed through untouched.
#[test]
fn a_configured_endpoint_is_the_request_url_verbatim() {
    const SENTINEL: &str = "https://endpoint.test/sentinel/path?x=1";

    for (kind, adapter) in [
        (
            "anthropic",
            Box::new(AnthropicAdapter::new("a", SENTINEL)) as Box<dyn Provider>,
        ),
        (
            "openai-compatible",
            Box::new(OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
                "o", SENTINEL,
            ))) as Box<dyn Provider>,
        ),
    ] {
        let transport = UrlCapturingTransport::default();
        // The stream is allowed to fail — an empty body is a truncated stream —
        // because the request has already been built and captured by then.
        let _ = block_on(adapter.stream_turn(sample_request(), &transport));
        let urls = transport.urls();
        assert_eq!(
            urls.len(),
            1,
            "the `{kind}` adapter issued {} requests for one turn; the capture below \
             assumes exactly one",
            urls.len()
        );
        assert_eq!(
            urls[0], SENTINEL,
            "the `{kind}` adapter was configured with `{SENTINEL}` and requested \
             `{}` instead. The configured endpoint is the whole request URL and is POSTed \
             as given: nothing may join a path, strip a query, or normalize a trailing \
             slash. Every vendor recipe the daemon ships is written against this contract \
             (BUG-170) — if an adapter ever needs to complete a URL, the recipes and their \
             seam test have to move first.",
            urls[0]
        );
    }
}
