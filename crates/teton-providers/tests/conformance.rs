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
use teton_providers::{
    AnthropicAdapter, FailureAction, Message, OpenAiCompatAdapter, OpenAiCompatConfig, Provider,
    ProviderError, Role, StopReason, ToolCall, ToolSpec, Transport, TransportError,
    TransportRequest, TransportResponse, TurnEvent, TurnRequest,
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
