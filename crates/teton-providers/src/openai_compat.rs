//! The OpenAI-compatible chat/completions adapter.
//!
//! Covers any endpoint speaking the OpenAI chat/completions streaming protocol —
//! DeepSeek, Kimi, Ollama, vLLM, and the like — with the endpoint configurable
//! so a new provider is registerable with no code change (BR-6). The response is
//! a stream of `data:` chunks terminated by `data: [DONE]`; `choices[0].delta`
//! carries text (`content`) and tool-call fragments (`tool_calls[].function.
//! arguments`, accumulated per index), and a final usage-only chunk carries
//! `prompt_tokens` / `completion_tokens`. All of it normalizes to the same
//! [`TurnEvent`]s the Anthropic adapter emits, in the same order.
//!
//! Because compat endpoints vary wildly in tool-call reliability, the default
//! capability tier is conservative (`Degraded`, i.e. the reduced BR-6 harness);
//! callers that trust a specific endpoint raise it explicitly.

use crate::capability::CapabilityProfile;
use crate::sse::{SseEvent, SseFramer};
use crate::transport::{ByteStream, HttpMethod, Transport, TransportRequest};
use crate::{
    finalize_tool, PartialTool, Provider, ProviderError, StopReason, TokenUsage, TurnCompletion,
    TurnEvent, TurnRequest, TurnStream,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use teton_core::ToolCallTier;

/// Configuration for an OpenAI-compatible provider instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatConfig {
    /// Stable provider id (feeds `provider_degraded`).
    pub id: String,
    /// Absolute chat/completions endpoint URL (e.g.
    /// `https://api.deepseek.com/v1/chat/completions`).
    pub endpoint: String,
    /// Capability profile. Defaults to the conservative `Degraded` tier for an
    /// unknown compat endpoint; raise it for a trusted provider.
    pub capabilities: CapabilityProfile,
}

impl OpenAiCompatConfig {
    /// Create a config for `id` calling `endpoint`, with the conservative
    /// default capability profile (`Degraded` tier).
    #[must_use]
    pub fn new(id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            endpoint: endpoint.into(),
            capabilities: CapabilityProfile {
                tool_call_tier: ToolCallTier::Degraded,
                parallel_calls: false,
                max_context: 0,
            },
        }
    }

    /// Override the capability profile.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilityProfile) -> Self {
        self.capabilities = capabilities;
        self
    }
}

/// Adapter for any OpenAI-compatible chat/completions endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiCompatAdapter {
    config: OpenAiCompatConfig,
}

impl OpenAiCompatAdapter {
    /// Create an adapter from a config.
    #[must_use]
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self { config }
    }

    fn build_request(&self, req: &TurnRequest) -> Result<TransportRequest, ProviderError> {
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(system) = &req.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        for m in &req.messages {
            messages.push(json!({"role": m.role.openai_str(), "content": m.content.as_str()}));
        }

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            // Ask for usage in the terminal chunk so Completed always carries it
            // (BR-2).
            "stream_options": {"include_usage": true},
            "max_tokens": req.max_tokens,
        });
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let body = serde_json::to_vec(&body).map_err(|e| ProviderError::Build(e.to_string()))?;
        Ok(TransportRequest {
            method: HttpMethod::Post,
            url: self.config.endpoint.clone(),
            // Auth header is added by the Transport (egress), not here (BR-7).
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "text/event-stream".to_string()),
            ],
            body,
        })
    }
}

#[async_trait]
impl Provider for OpenAiCompatAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn capabilities(&self) -> CapabilityProfile {
        self.config.capabilities
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
        // Guarantee a terminal Completed even without a [DONE] marker.
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

/// Streaming assembly state for one OpenAI-compatible turn.
#[derive(Debug, Default)]
struct State {
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<StopReason>,
    tools: Vec<PartialTool>,
    flushed: bool,
    completed: bool,
}

impl State {
    fn step(&mut self, sse: &SseEvent) -> Result<Vec<TurnEvent>, ProviderError> {
        let data = sse.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            let mut out = self.flush_tools()?;
            if !self.completed {
                out.push(self.completed_event());
                self.completed = true;
            }
            return Ok(out);
        }

        let v: Value = serde_json::from_str(data).map_err(|_| ProviderError::MalformedResponse)?;
        let mut out = Vec::new();

        // A terminal usage-only chunk (choices empty) carries token counts.
        if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
            if let Some(i) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = i;
            }
            if let Some(o) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = o;
            }
        }

        if let Some(choice) = v.pointer("/choices/0") {
            if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
                if !content.is_empty() {
                    out.push(TurnEvent::TextDelta(content.to_string()));
                }
            }
            if let Some(calls) = choice
                .pointer("/delta/tool_calls")
                .and_then(Value::as_array)
            {
                for call in calls {
                    self.accumulate_tool_call(call);
                }
            }
            if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop_reason = Some(StopReason::from_token(finish));
                // Tool calls are complete once a finish reason arrives; flush
                // them now (usage/Completed still comes at [DONE] or stream end).
                out.extend(self.flush_tools()?);
            }
        }

        Ok(out)
    }

    /// Merge one `tool_calls[]` fragment into the per-index accumulator.
    fn accumulate_tool_call(&mut self, call: &Value) {
        let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.tools.len() <= idx {
            self.tools.push(PartialTool::default());
        }
        let slot = &mut self.tools[idx];
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                slot.id = id.to_string();
            }
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            if !name.is_empty() {
                slot.name = name.to_string();
            }
        }
        if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
            slot.args.push_str(args);
        }
    }

    fn flush_tools(&mut self) -> Result<Vec<TurnEvent>, ProviderError> {
        if self.flushed || self.tools.is_empty() {
            return Ok(Vec::new());
        }
        self.flushed = true;
        let mut out = Vec::with_capacity(self.tools.len());
        for tool in std::mem::take(&mut self.tools) {
            out.push(finalize_tool(tool)?);
        }
        Ok(out)
    }

    fn finalize(&mut self) -> Result<Vec<TurnEvent>, ProviderError> {
        let mut out = self.flush_tools()?;
        if !self.completed {
            out.push(self.completed_event());
            self.completed = true;
        }
        Ok(out)
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
