//! teton-providers — provider adapters.
//!
//! Adapters for the Anthropic Messages API and any OpenAI-compatible
//! chat/completions endpoint (DeepSeek, Kimi, Ollama, vLLM, …). The crate turns
//! two very different wire protocols into **one** normalized turn stream
//! ([`TurnEvent`]) so the harness never sees provider-specific shapes.
//!
//! ## The load-bearing decision (architecture D-2)
//!
//! Adapters do **no** I/O of their own. [`Provider::stream_turn`] is handed a
//! [`Transport`] — implemented by `tetond`'s single egress choke point — and
//! calls *that* to reach the network. This crate has no HTTP client dependency
//! (verify with `cargo tree`), which is precisely what makes the privacy
//! boundary (BR-1) and cost recording (BR-2) enforceable at one point instead
//! of being re-implemented (and forgotten) in every adapter.
//!
//! The [`Transport`] is also responsible for **authentication**: it resolves
//! the keychain reference and attaches the credential header. Adapters build the
//! semantic request (URL, body, protocol headers) and never see a raw secret
//! (BR-7).
//!
//! ## Module map
//! - [`transport`] — the [`Transport`] trait adapters call (D-2).
//! - [`anthropic`] — the Anthropic Messages SSE adapter.
//! - [`openai_compat`] — the OpenAI-compatible chat/completions adapter.
//! - [`capability`] — [`CapabilityProfile`] and the BR-6 degradation mapping.
//! - [`failure`] — failure classification feeding the `provider_degraded` event.

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod capability;
pub mod failure;
pub mod openai_compat;
pub mod transport;

mod sse;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

pub use anthropic::AnthropicAdapter;
pub use capability::{CapabilityProfile, HarnessProfile};
pub use failure::{
    classify, degradation_signal, FailureAction, FailureClass, FailureDecision, ProviderDegraded,
};
pub use openai_compat::{OpenAiCompatAdapter, OpenAiCompatConfig};
pub use transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

/// A single normalized event emitted while a turn streams in. Both adapters emit
/// events in the same order: any number of [`TurnEvent::TextDelta`], then any
/// assembled [`TurnEvent::ToolCall`]s, then exactly one terminal
/// [`TurnEvent::Completed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// An incremental chunk of assistant text.
    TextDelta(String),
    /// A fully assembled tool call. Emitted only once its argument fragments
    /// have been concatenated and parsed into valid JSON.
    ToolCall(ToolCall),
    /// The terminal event of a successful turn. Always carries token usage
    /// (BR-2) and the stop reason.
    Completed(TurnCompletion),
}

/// A normalized tool call. Both providers' wire formats (Anthropic
/// `tool_use` blocks and OpenAI `tool_calls` fragments) collapse to this shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id, echoed back with the tool result.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Parsed argument object. Never a raw string — malformed argument JSON is
    /// surfaced as [`ProviderError::MalformedToolCall`] instead.
    pub arguments: Value,
}

/// The terminal payload of a completed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCompletion {
    /// Token usage for the turn (always populated — BR-2).
    pub usage: TokenUsage,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
}

/// Token counts for one completed turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt / input tokens.
    pub input_tokens: u64,
    /// Completion / output tokens.
    pub output_tokens: u64,
}

/// Normalized stop reason across providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn normally.
    EndTurn,
    /// The model stopped to make one or more tool calls.
    ToolUse,
    /// The model hit the output-token limit.
    MaxTokens,
    /// Any other provider-specific reason, kept verbatim.
    Other(String),
}

impl StopReason {
    /// Normalize a raw provider stop/finish token to a [`StopReason`].
    ///
    /// Covers both vocabularies: Anthropic (`end_turn`, `tool_use`,
    /// `max_tokens`) and OpenAI (`stop`, `tool_calls`, `length`,
    /// `function_call`). Unknown tokens are preserved as
    /// [`StopReason::Other`].
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "end_turn" | "stop" | "stop_sequence" => StopReason::EndTurn,
            "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
            "max_tokens" | "length" => StopReason::MaxTokens,
            other => StopReason::Other(other.to_string()),
        }
    }
}

/// The role of a message in a turn request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System / developer instructions.
    System,
    /// End-user input.
    User,
    /// Prior assistant output.
    Assistant,
    /// A tool result fed back to the model.
    Tool,
}

impl Role {
    /// The OpenAI chat-completions role string.
    pub(crate) fn openai_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// One message in a turn request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Speaker role.
    pub role: Role,
    /// Message text.
    pub content: String,
}

/// A tool the model may call, in a provider-agnostic shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// A provider-agnostic request for one streamed turn. Each adapter maps this to
/// its provider's wire body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRequest {
    /// Concrete model name (e.g. `claude-3-5-sonnet`, `deepseek-chat`).
    pub model: String,
    /// Optional top-level system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Conversation so far.
    pub messages: Vec<Message>,
    /// Tools available this turn.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Maximum output tokens.
    pub max_tokens: u32,
}

/// A pinned, boxed stream of normalized turn events. `Send` so the daemon can
/// drive it from any task.
pub type TurnStream = Pin<Box<dyn Stream<Item = Result<TurnEvent, ProviderError>> + Send>>;

/// A provider adapter: turns a [`TurnRequest`] into a normalized [`TurnStream`],
/// reaching the network only through the injected [`Transport`] (D-2).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id of this provider instance (feeds `provider_degraded`).
    fn id(&self) -> &str;

    /// This provider's capability profile (drives BR-6 harness degradation).
    fn capabilities(&self) -> CapabilityProfile;

    /// Start a streamed turn.
    ///
    /// Errors known at open time (timeout, 4xx, 5xx) are returned here so the
    /// caller can retry/fallback before any events flow. Errors discovered mid
    /// stream (a truncated body, a malformed tool call) are yielded as an `Err`
    /// item in the returned stream. Either way the caller can call
    /// [`ProviderError::decision`] to get a fallback/degrade/retry decision.
    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError>;
}

/// An error from a provider adapter. Every variant except [`ProviderError::Build`]
/// maps to a [`FailureClass`] so the daemon can decide retry / fallback /
/// degrade uniformly (AC-7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The transport timed out opening or reading the response.
    #[error("provider request timed out")]
    Timeout,
    /// A transport-level failure (connection reset, DNS, …) that is not a
    /// timeout.
    #[error("provider transport error")]
    Transport,
    /// The provider returned a 4xx status.
    #[error("provider returned client error status {status}")]
    ClientError {
        /// HTTP status code.
        status: u16,
    },
    /// The provider returned a 5xx status.
    #[error("provider returned server error status {status}")]
    ServerError {
        /// HTTP status code.
        status: u16,
    },
    /// The response stream was not parseable as the expected protocol.
    #[error("malformed provider response")]
    MalformedResponse,
    /// A tool call's assembled arguments were not valid JSON. Classified and
    /// surfaced — never a panic (AC).
    #[error("malformed tool-call arguments for tool `{tool}`")]
    MalformedToolCall {
        /// The offending tool's name.
        tool: String,
    },
    /// The request could not be built (serialization / configuration problem).
    /// This is a local programmer/config error, not a provider failure, so it
    /// has no [`FailureClass`].
    #[error("failed to build provider request: {0}")]
    Build(String),
}

impl ProviderError {
    /// Map to a [`FailureClass`], or `None` for [`ProviderError::Build`] (a
    /// local error, not a provider failure).
    #[must_use]
    pub fn failure_class(&self) -> Option<FailureClass> {
        Some(match self {
            ProviderError::Timeout => FailureClass::Timeout,
            ProviderError::Transport => FailureClass::Transport,
            ProviderError::ClientError { status } => FailureClass::ClientError { status: *status },
            ProviderError::ServerError { status } => FailureClass::ServerError { status: *status },
            ProviderError::MalformedResponse => FailureClass::MalformedResponse,
            ProviderError::MalformedToolCall { .. } => FailureClass::MalformedToolCall,
            ProviderError::Build(_) => return None,
        })
    }

    /// The retry / fallback / degrade decision for this error, or `None` for a
    /// local [`ProviderError::Build`].
    #[must_use]
    pub fn decision(&self) -> Option<FailureDecision> {
        self.failure_class().map(classify)
    }

    /// Translate a transport-level error into a provider error.
    pub(crate) fn from_transport(err: TransportError) -> Self {
        match err {
            TransportError::Timeout => ProviderError::Timeout,
            TransportError::Connect | TransportError::Io => ProviderError::Transport,
        }
    }
}

/// A tool call being assembled from streamed argument fragments.
#[derive(Debug, Default)]
pub(crate) struct PartialTool {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: String,
}

/// Finalize an assembled tool call: parse its argument fragments as JSON.
///
/// Empty arguments normalize to `{}`. Invalid JSON is surfaced as
/// [`ProviderError::MalformedToolCall`] — the tool's `name` never contains user
/// content, so it is safe to include in the error.
pub(crate) fn finalize_tool(tool: PartialTool) -> Result<TurnEvent, ProviderError> {
    let raw = if tool.args.trim().is_empty() {
        "{}"
    } else {
        tool.args.as_str()
    };
    let arguments = serde_json::from_str(raw).map_err(|_| ProviderError::MalformedToolCall {
        tool: tool.name.clone(),
    })?;
    Ok(TurnEvent::ToolCall(ToolCall {
        id: tool.id,
        name: tool.name,
        arguments,
    }))
}

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }

    #[test]
    fn stop_reason_normalizes_both_vocabularies() {
        assert_eq!(StopReason::from_token("end_turn"), StopReason::EndTurn);
        assert_eq!(StopReason::from_token("stop"), StopReason::EndTurn);
        assert_eq!(StopReason::from_token("tool_use"), StopReason::ToolUse);
        assert_eq!(StopReason::from_token("tool_calls"), StopReason::ToolUse);
        assert_eq!(StopReason::from_token("max_tokens"), StopReason::MaxTokens);
        assert_eq!(StopReason::from_token("length"), StopReason::MaxTokens);
        assert_eq!(
            StopReason::from_token("content_filter"),
            StopReason::Other("content_filter".to_string())
        );
    }

    #[test]
    fn provider_error_maps_to_failure_class() {
        assert_eq!(
            ProviderError::Timeout.failure_class(),
            Some(FailureClass::Timeout)
        );
        assert_eq!(
            ProviderError::ClientError { status: 404 }.failure_class(),
            Some(FailureClass::ClientError { status: 404 })
        );
        // Build is a local error, not a provider failure.
        assert_eq!(ProviderError::Build("x".into()).failure_class(), None);
        assert_eq!(ProviderError::Build("x".into()).decision(), None);
    }

    #[test]
    fn empty_tool_args_normalize_to_empty_object() {
        let ev = finalize_tool(PartialTool {
            id: "t1".into(),
            name: "noop".into(),
            args: "   ".into(),
        })
        .expect("empty args are valid");
        match ev {
            TurnEvent::ToolCall(tc) => assert_eq!(tc.arguments, serde_json::json!({})),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn malformed_tool_args_are_classified_never_panic() {
        let err = finalize_tool(PartialTool {
            id: "t1".into(),
            name: "get_weather".into(),
            args: "{\"city\":".into(),
        })
        .expect_err("truncated JSON must be an error");
        assert_eq!(
            err,
            ProviderError::MalformedToolCall {
                tool: "get_weather".into()
            }
        );
        assert_eq!(
            err.decision().map(|d| d.action),
            Some(FailureAction::Degrade)
        );
    }
}
