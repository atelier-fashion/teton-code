//! The Anthropic Messages API adapter.
//!
//! Maps a [`TurnRequest`] to the Messages request body and parses the streaming
//! SSE response (`message_start`, `content_block_*`, `message_delta`,
//! `message_stop`) into the crate's normalized [`TurnEvent`]s. Token usage is
//! assembled from `message_start` (input) and `message_delta` (output) so a
//! [`TurnEvent::Completed`] always carries usage (BR-2). Tool-use blocks are
//! accumulated from `input_json_delta` fragments and parsed once; malformed
//! argument JSON is surfaced as [`ProviderError::MalformedToolCall`], never a
//! panic.

use crate::capability::CapabilityProfile;
use crate::sse::{SseEvent, SseFramer};
use crate::transport::{ByteStream, HttpMethod, Transport, TransportRequest};
use crate::{
    finalize_tool, PartialTool, Provider, ProviderError, Role, StopReason, TokenUsage,
    TurnCompletion, TurnEvent, TurnRequest, TurnStream,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use teton_core::ToolCallTier;

/// Default Anthropic context window used when none is configured.
const DEFAULT_MAX_CONTEXT: u32 = 200_000;
/// The Messages API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Adapter for the Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    id: String,
    /// Absolute Messages endpoint URL (e.g. `https://api.anthropic.com/v1/messages`).
    endpoint: String,
    capabilities: CapabilityProfile,
}

impl AnthropicAdapter {
    /// Create an adapter for a provider `id` calling the given Messages
    /// `endpoint` URL. Anthropic has reliable native tool-calling, so the
    /// default profile is `Native`.
    #[must_use]
    pub fn new(id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            endpoint: endpoint.into(),
            capabilities: CapabilityProfile {
                tool_call_tier: ToolCallTier::Native,
                parallel_calls: true,
                max_context: DEFAULT_MAX_CONTEXT,
            },
        }
    }

    /// Override the capability profile (e.g. for a proxied or older model).
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilityProfile) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn build_request(&self, req: &TurnRequest) -> Result<TransportRequest, ProviderError> {
        let mut system = req.system.clone();
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            match m.role {
                // Anthropic carries the system prompt at the top level, not as a
                // message; hoist any system-role messages there.
                Role::System => match system.as_mut() {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(&m.content);
                    }
                    None => system = Some(m.content.clone()),
                },
                Role::Assistant => {
                    messages.push(json!({"role": "assistant", "content": m.content.as_str()}));
                }
                // Tool results are fed back as user turns in the MVP mapping.
                Role::User | Role::Tool => {
                    messages.push(json!({"role": "user", "content": m.content.as_str()}));
                }
            }
        }

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
            "stream": true,
        });
        if let Some(system) = system {
            body["system"] = json!(system);
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let body = serde_json::to_vec(&body).map_err(|e| ProviderError::Build(e.to_string()))?;
        Ok(TransportRequest {
            method: HttpMethod::Post,
            url: self.endpoint.clone(),
            // Auth headers are added by the Transport (egress), not here (BR-7).
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "text/event-stream".to_string()),
                (
                    "anthropic-version".to_string(),
                    ANTHROPIC_VERSION.to_string(),
                ),
            ],
            body,
        })
    }
}

#[async_trait]
impl Provider for AnthropicAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> CapabilityProfile {
        self.capabilities
    }

    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError> {
        let http = self.build_request(&request)?;
        let resp = transport
            .execute(http)
            .await
            .map_err(ProviderError::from_transport)?;
        if resp.status >= 500 {
            return Err(ProviderError::ServerError {
                status: resp.status,
            });
        }
        if resp.status >= 400 {
            return Err(ProviderError::ClientError {
                status: resp.status,
            });
        }
        Ok(event_stream(resp.body))
    }
}

/// Build the normalized event stream from the raw SSE byte stream.
fn event_stream(body: ByteStream) -> TurnStream {
    Box::pin(async_stream::stream! {
        let mut framer = SseFramer::new();
        let mut state = State::default();
        let mut body = body;
        while let Some(chunk) = body.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(_) => {
                    yield Err(ProviderError::Transport);
                    return;
                }
            };
            for sse in framer.push(&bytes) {
                match state.step(&sse) {
                    Ok(events) => {
                        for e in events {
                            yield Ok(e);
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
        }
        if let Some(sse) = framer.finish() {
            match state.step(&sse) {
                Ok(events) => {
                    for e in events {
                        yield Ok(e);
                    }
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
        // Guarantee a terminal Completed even if the provider cut the stream
        // before message_stop (usage still carried — BR-2).
        match state.finalize() {
            Ok(events) => {
                for e in events {
                    yield Ok(e);
                }
            }
            Err(e) => yield Err(e),
        }
    })
}

/// Streaming assembly state for one Anthropic turn.
#[derive(Debug, Default)]
struct State {
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<StopReason>,
    active_tool: Option<PartialTool>,
    completed: bool,
}

impl State {
    fn step(&mut self, sse: &SseEvent) -> Result<Vec<TurnEvent>, ProviderError> {
        let data = sse.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(data).map_err(|_| ProviderError::MalformedResponse)?;
        let kind = sse
            .event
            .as_deref()
            .or_else(|| v.get("type").and_then(Value::as_str))
            .unwrap_or_default();

        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(usage) = v.pointer("/message/usage") {
                    if let Some(i) = usage.get("input_tokens").and_then(Value::as_u64) {
                        self.input_tokens = i;
                    }
                    if let Some(o) = usage.get("output_tokens").and_then(Value::as_u64) {
                        self.output_tokens = o;
                    }
                }
            }
            "content_block_start" => {
                if let Some(block) = v.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.active_tool = Some(PartialTool {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            args: String::new(),
                        });
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = v.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                out.push(TurnEvent::TextDelta(t.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let (Some(tool), Some(p)) = (
                                self.active_tool.as_mut(),
                                delta.get("partial_json").and_then(Value::as_str),
                            ) {
                                tool.args.push_str(p);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(tool) = self.active_tool.take() {
                    out.push(finalize_tool(tool)?);
                }
            }
            "message_delta" => {
                if let Some(o) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.output_tokens = o;
                }
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(StopReason::from_token(sr));
                }
            }
            "message_stop" => {
                out.push(self.completed_event());
                self.completed = true;
            }
            "error" => return Err(ProviderError::MalformedResponse),
            _ => {} // ping and other events are ignored.
        }
        Ok(out)
    }

    fn finalize(&mut self) -> Result<Vec<TurnEvent>, ProviderError> {
        if self.completed {
            return Ok(Vec::new());
        }
        self.completed = true;
        Ok(vec![self.completed_event()])
    }

    fn completed_event(&self) -> TurnEvent {
        TurnEvent::Completed(TurnCompletion {
            usage: TokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
            stop_reason: self.stop_reason.clone().unwrap_or(StopReason::EndTurn),
        })
    }
}
