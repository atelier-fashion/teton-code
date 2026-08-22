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
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

pub use anthropic::AnthropicAdapter;
pub use capability::{CapabilityProfile, HarnessProfile};
// REQ-559: re-exported, never redefined. One ladder, one clamp, one resolver
// (BR-3) — an adapter-local copy of this vocabulary would be a second place for
// the wire spellings to drift from the router's.
pub use failure::{
    classify, degradation_signal, FailureAction, FailureClass, FailureDecision, ProviderDegraded,
};
pub use openai_compat::{OpenAiCompatAdapter, OpenAiCompatConfig};
pub use teton_core::{EffortLevel, EffortOmission, ReasoningShape, ResolvedEffort};
pub use transport::{
    BlockDetail, ByteStream, HttpMethod, Transport, TransportError, TransportRequest,
    TransportResponse,
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
    /// Of [`Self::output_tokens`], how many the provider attributes to
    /// reasoning (REQ-559 BR-10).
    ///
    /// A **component of** `output_tokens`, never an addition to it: both
    /// providers' aggregate counts already include reasoning tokens, so this is
    /// an attribution change and not a totals change. Summing the two would
    /// double-count and inflate every reported figure — for a product whose
    /// headline promise is cost control, worse than not reporting the split.
    ///
    /// `None` means **unreported**, never `0`. A provider that does not tell us
    /// is a different fact from one that reports zero reasoning, and collapsing
    /// them would put an estimate where an actual belongs (REQ-544 BR-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
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
    /// What this call puts in its reasoning field(s) (REQ-559).
    ///
    /// **Required, with no `#[serde(default)]`, and [`ResolvedEffort`]
    /// implements no [`Default`]** — that is the point (REQ-559 ADR-B). BR-1
    /// says omitting effort is never a valid outcome, because omission inherits
    /// the provider's default and at least one target provider defaults to
    /// `max`. A test enumerating call paths is the guard LESSON-443 describes:
    /// correct only until someone adds another path. Rust struct-literal syntax
    /// requires every field, so a construction site that has not thought about
    /// effort does not compile.
    ///
    /// The level here is **already clamped** — resolved once at route time
    /// (ADR-G). Adapters `match` this and never re-clamp.
    pub effort: ResolvedEffort,
}

/// How many bytes of a 400 response body are read to decide whether it is a
/// REQ-559 BR-12 effort refusal or a REQ-586 BR-2 context-length refusal.
///
/// Bounded because the body is provider-controlled and this is an error path:
/// a provider that answered 400 with a megabyte of prose must not cost the
/// daemon a megabyte of allocation to classify. Every observed vendor error body
/// names the offending parameter in its first few hundred bytes.
const EFFORT_REFUSAL_SNIFF_BYTES: usize = 4096;

/// The OpenAI-compatible `error.code` value for a request that exceeds the
/// model's context window, as the quoted JSON string token so that both the
/// compact (`"code":"context_length_exceeded"`) and the pretty-printed
/// (`"code": "context_length_exceeded"`) bodies OpenAI and its imitators send
/// match. Verified against vendor docs 2026-08-19 (OpenAI chat/completions 400
/// `invalid_request_error`, `param: "messages"`, `code:
/// "context_length_exceeded"`).
const OPENAI_CONTEXT_LENGTH_CODE: &str = "\"context_length_exceeded\"";

/// The OpenAI-compatible message spelling: `This model's maximum context length
/// is N tokens. However, …`. Carried by endpoints that reuse OpenAI's wording
/// but not its `code` (DeepSeek, vLLM). Verified against vendor docs 2026-08-19.
const OPENAI_CONTEXT_LENGTH_MESSAGE: &str = "maximum context length";

/// The Anthropic Messages API spelling: a 400 `invalid_request_error` whose
/// message is `prompt is too long: N tokens > M maximum`. Verified against
/// vendor docs 2026-08-19 (platform.claude.com, "Context window overflow
/// behavior": "If the input alone already exceeds the model's context window,
/// the API returns a 400 `invalid_request_error` ("prompt is too long")").
const ANTHROPIC_CONTEXT_LENGTH_MESSAGE: &str = "prompt is too long";

/// The Moonshot/Kimi platform spelling: a 400 `invalid_request_error` whose
/// message is `Input token length too long`, glossed by the vendor as "The
/// input tokens exceed the model's maximum context limit". Verified against
/// vendor docs 2026-08-19 (<https://platform.kimi.ai/docs/api/errors>, "Common
/// Error Codes"; the CN mirror is `platform.kimi.com/docs/api/errors`).
///
/// Pinned because **Kimi is the dogfood provider** and Moonshot does not send
/// either OpenAI spelling: `context_length_exceeded` appears nowhere in their
/// published docs corpus or their OpenAPI document, so without this const a
/// Kimi overflow would fall through to `ClientError { 400 }` — a health
/// downgrade and a failover for a request that is simply too big, which is the
/// exact outcome BR-2 exists to prevent.
///
/// The leading `Input` is deliberately **not** part of the match: the vendor's
/// table heads the column "Typical message" rather than promising the byte
/// string, so the fragment is the part that carries the meaning and survives a
/// re-worded prefix — the same posture as
/// [`OPENAI_CONTEXT_LENGTH_MESSAGE`], which is likewise a fragment of a longer
/// sentence.
///
/// Moonshot's *Kimi Code* subscription endpoint (`kimi-for-coding`) words the
/// same refusal differently — `Invalid request: Your request exceeded model
/// token limit: N (requested: M)` — and is left unpinned on purpose: it is a
/// different product surface from the OpenAI-compatible
/// `/v1/chat/completions` this crate's adapters call, and ADR-8's narrowness
/// rule says a spelling is pinned when an adapter can actually receive it.
const MOONSHOT_CONTEXT_LENGTH_MESSAGE: &str = "token length too long";

/// The llama.cpp `llama-server` spelling: a 400 whose body is
/// `{"error":{"code":400,"message":"the request exceeds the available context
/// size. try increasing the context size or enable context shift",
/// "type":"exceed_context_size_error","n_prompt_tokens":…,"n_ctx":…}}`.
/// Verified 2026-08-19 against two independent reproductions of the real body
/// (`continuedev/continue` #9797 — HTTP 400, "the request exceeds the available
/// context size, try increasing it"; `oobabooga/textgen` #7257 — the JSON
/// above), the wording differing between builds only in the trailing advice.
///
/// Pinned because the **local-engine class is what
/// `budget::MIN_BUDGET_BYTES` names as the live sub-floor case**: a route whose
/// declared window derives below the floor runs under a budget larger than it
/// declared, and the floor's whole justification is that the provider's typed
/// refusal is what reports the overflow. `llama-server` behind an
/// OpenAI-compatible endpoint sends none of the four spellings above, so without
/// this const that route took `ClientError { 400 }` → `Fallback` → a health
/// downgrade for a request that was simply too big.
///
/// The fragment is the middle of the sentence, not its start: the two observed
/// builds differ in the tail ("try increasing it" vs "try increasing the
/// context size or enable context shift") and the type token
/// (`exceed_context_size_error`) is newer than the message. `exceeds the
/// available context size` is the part both carry and the part that means it —
/// the [`OPENAI_CONTEXT_LENGTH_MESSAGE`] posture.
///
/// **Not a complete backstop for this class, and the floor's docs say so.**
/// Two gaps are known and neither is guessed at here:
///
/// * some `llama-server` builds put `400` in the JSON while answering with a
///   different HTTP status ("the actual HTTP status code is not 400" —
///   `oobabooga/textgen` #7257), and the sniff is deliberately
///   [`TYPED_REFUSAL_STATUS`]-only, so those bodies stay
///   `ClientError`;
/// * **Ollama** — the shipped 4,096 recipe `MIN_BUDGET_BYTES` cites — is a
///   different server from `llama-server` and its documented
///   `/v1/chat/completions` behaviour on an over-long prompt is to *truncate*
///   rather than refuse. There is no wording to pin, so none is invented.
const LLAMA_CPP_CONTEXT_LENGTH_MESSAGE: &str = "exceeds the available context size";

/// The **only** status either typed refusal is read out of — a context-length
/// refusal (REQ-586 BR-2, verify M3) or an effort refusal (REQ-559 BR-12).
///
/// Every spelling either sniff matches was read off a `400 Bad Request` body:
/// a request that overflows the model's window is a malformed request, and so is
/// one naming a reasoning field the endpoint does not take. There is no upstream
/// that reports either as anything else. The status is therefore part of the
/// claim, not incidental to it, and **both** sniffs are gated on it — the effort
/// half arrived one fix pass later than the context-length half, which is the
/// same defect in the same shape: a sniff that reads a body without asking what
/// the status meant.
///
/// The failure this closes is specific and bad in both directions. Both variants
/// this classifier returns carry **no** [`FailureClass`], so they skip
/// `record_health`, `on_provider_failure` and failover entirely — that is
/// exactly right for a 400 the provider answered correctly, and exactly wrong
/// for anything else:
///
/// - A **401/403** — a revoked, wrong, or expired key — whose body happens to
///   contain `maximum context length` or `prompt is too long` would be reported
///   to the user as "your context is too big", sending them to shrink a
///   conversation when the fix is their credential.
/// - A **429** would lose [`FailureAction::Retry`]: no backoff, no rate-limit
///   degradation, just a class-less refusal.
/// - And because none of those change the provider's standing, a misbehaving or
///   hostile endpoint could keep itself in the route indefinitely by answering
///   *any* 4xx with "prompt is too long" — every subsequent turn's full
///   assembled context still going to it. `reasoning_effort` in a 401 body buys
///   the same immunity, and additionally poisons the session's effort memo, so
///   the provider silently stops being sent a level it accepts (BR-6's
///   misattribution family).
///
/// That is not a hypothetical body shape: gateways (LiteLLM, OpenRouter, vLLM)
/// relay an upstream vendor's error document under a status of their own
/// choosing, so a context-length sentence under a 401 or a 429 is a body these
/// adapters can really receive.
const TYPED_REFUSAL_STATUS: u16 = 400;

/// Whether a 400 body names the reasoning field this request sent (REQ-559
/// BR-12).
///
/// Deliberately narrow. The memo this feeds (ADR-F) stops sending effort to a
/// provider for the rest of the session, so a false positive is a silent
/// downgrade — exactly the failure mode BR-6 exists to prevent. It therefore
/// matches the **field names this crate actually emits** rather than anything
/// resembling an effort complaint, and an unrelated 400 stays a `ClientError`.
#[must_use]
pub fn body_names_the_effort_field(body: &[u8]) -> bool {
    let head = &body[..body.len().min(EFFORT_REFUSAL_SNIFF_BYTES)];
    let Ok(text) = std::str::from_utf8(head) else {
        // A non-UTF-8 error body tells us nothing. Not an effort refusal —
        // "unclassifiable" must fall to the general 4xx path, not to the memo.
        return false;
    };
    // The two spellings the adapters in this crate emit, and nothing else.
    text.contains("reasoning_effort") || text.contains("output_config")
}

/// Whether a 400 body says the request exceeded the model's context window
/// (REQ-586 BR-2 / ADR-8).
///
/// The [`body_names_the_effort_field`] posture: **exact vendor spellings only**,
/// no general parse. A false positive here turns an ordinary client error into
/// a typed outcome the daemon neither retries nor fails over nor counts against
/// the provider's health — so the sniff matches the five spellings the four
/// upstreams actually send and nothing that merely resembles a size complaint.
/// The set is what has been **verified**, never what seems likely: a provider
/// whose refusal is worded some other way keeps `ClientError`, and
/// `budget::MIN_BUDGET_BYTES` names the one that does.
/// Like the effort sniff it reads only the bounded `read_error_head` prefix; the
/// matched text never leaves this function (conventions: no provider prose in
/// errors).
#[must_use]
pub fn body_names_context_length(body: &[u8]) -> bool {
    let head = &body[..body.len().min(EFFORT_REFUSAL_SNIFF_BYTES)];
    let Ok(text) = std::str::from_utf8(head) else {
        // A non-UTF-8 error body tells us nothing. Not a context-length
        // refusal — "unclassifiable" falls to the general 4xx path.
        return false;
    };
    text.contains(OPENAI_CONTEXT_LENGTH_CODE)
        || text.contains(OPENAI_CONTEXT_LENGTH_MESSAGE)
        || text.contains(ANTHROPIC_CONTEXT_LENGTH_MESSAGE)
        || text.contains(MOONSHOT_CONTEXT_LENGTH_MESSAGE)
        || text.contains(LLAMA_CPP_CONTEXT_LENGTH_MESSAGE)
}

/// Read a bounded prefix of an error body so a 400 can be classified (REQ-559
/// BR-12, REQ-586 BR-2), then discard the rest.
///
/// Only ever called on a 4xx, where the body is an error document rather than a
/// turn. The cap is [`EFFORT_REFUSAL_SNIFF_BYTES`]; a stream error while reading
/// yields whatever arrived, because a partial error body still classifies and a
/// read failure on an already-failed request is not worth a second error.
async fn read_error_head(mut body: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < EFFORT_REFUSAL_SNIFF_BYTES {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk),
            Some(Err(_)) | None => break,
        }
    }
    out.truncate(EFFORT_REFUSAL_SNIFF_BYTES);
    out
}

/// The classification for a 4xx: a REQ-559 BR-12 effort refusal, a REQ-586
/// BR-2 context-length refusal, or the general client error. Shared by both
/// adapters so each rule has one implementation.
///
/// `sent` is what the request actually carried, and it names **both** levels —
/// which is what lets this layer satisfy BR-12's "names the provider, the
/// requested level, and the clamped level" without ever seeing the setting.
///
/// A provider that was sent **no** reasoning field cannot be refusing one, so
/// the effort check short-circuits. That is also what makes the daemon's retry
/// non-looping by construction: the retry sends `Omit`, and an `Omit` request
/// can never be classified as an effort refusal.
///
/// The bounded head is read for **every** 4xx, not only when effort was sent:
/// the request that overflows the window may well be the `Omit` retry of an
/// effort refusal, and a context-length refusal on that path must still come
/// back typed (ADR-8). The effort sniff runs first — it is the more specific
/// claim and the only one that needs `sent` — then the context-length sniff.
///
/// Both are gated on the status being exactly [`TYPED_REFUSAL_STATUS`], by one
/// check ahead of them rather than a condition on each — see there for why a
/// 401, a 403 or a 429 carrying either sentence must keep the classification it
/// already had.
pub(crate) async fn classify_client_error(
    status: u16,
    body: ByteStream,
    provider_id: &str,
    sent: ResolvedEffort,
) -> ProviderError {
    let head = read_error_head(body).await;
    // One gate, ahead of both sniffs: neither typed outcome may be reached from
    // a status that means something else. See [`TYPED_REFUSAL_STATUS`].
    if status != TYPED_REFUSAL_STATUS {
        return ProviderError::ClientError { status };
    }
    if let ResolvedEffort::Effort { level, requested } = sent {
        if body_names_the_effort_field(&head) {
            return ProviderError::EffortRefused {
                provider_id: provider_id.to_owned(),
                requested,
                clamped: level,
            };
        }
    }
    if body_names_context_length(&head) {
        return ProviderError::ContextLengthExceeded {
            provider_id: provider_id.to_owned(),
        };
    }
    ProviderError::ClientError { status }
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
    /// The provider refused the reasoning-effort field (REQ-559 BR-12).
    ///
    /// A 400 whose body names the effort field, and **only** that. An unrelated
    /// 400 stays a [`ProviderError::ClientError`] — narrow on purpose, because
    /// this error is what populates the session refusal memo (ADR-F), and a
    /// memo poisoned by an unrelated failure would stop sending effort to a
    /// provider that accepts it, silently, for the rest of the session.
    ///
    /// Names all three values BR-12 requires: the provider, the level the user
    /// asked for, and the level the clamp actually sent. It carries **no
    /// response body and no prompt text** — conventions.md forbids content in
    /// error messages.
    ///
    /// It has no [`FailureClass`]: it is not a failure the retry / fallback /
    /// degrade machinery should act on. The one correct response is a single
    /// retry with no reasoning field, which the daemon performs — and then
    /// remembers, so the failing request is not made again this session.
    #[error(
        "provider `{provider_id}` refused the reasoning-effort field \
         (requested `{requested}`, sent `{clamped}`)"
    )]
    EffortRefused {
        /// The provider that refused.
        provider_id: String,
        /// The level the user set, before clamping.
        requested: EffortLevel,
        /// The level actually sent, after the per-provider clamp.
        clamped: EffortLevel,
    },
    /// The provider refused the request as exceeding its context window
    /// (REQ-586 BR-2 / ADR-8).
    ///
    /// A **400** — [`TYPED_REFUSAL_STATUS`], the only status any vendor
    /// reports an overflowed window under — whose body carries one of the exact
    /// vendor spellings in [`body_names_context_length`], and **only** that. An
    /// unrelated 400, and *any* other 4xx whatever its body says, stays a
    /// [`ProviderError::ClientError`] — narrow on purpose, in the
    /// [`ProviderError::EffortRefused`] posture, because this error bypasses the
    /// retry / fallback / degrade machinery entirely and a misread 4xx would
    /// hide a real provider fault (a revoked key, a rate limit) behind a budget
    /// report, with no health change to move later turns off it.
    ///
    /// It names the provider and nothing else: the window and the assembled
    /// size are facts the daemon holds (the route budget), not facts the adapter
    /// can see, so the daemon's `HarnessError::ContextLengthExceeded` is where
    /// they are added. It carries **no response body and no prompt text** —
    /// conventions.md forbids provider prose in error messages.
    ///
    /// It has no [`FailureClass`]: the request is too big for the window, which
    /// retrying or failing over cannot fix — a fallback provider with the same
    /// or a smaller window would refuse the same bytes, and a provider that
    /// correctly reported its limit is not unhealthy. The harness reports it
    /// as a typed outcome naming the window and the assembled size; no health
    /// change, no `on_provider_failure`.
    #[error("provider `{provider_id}` refused the request as exceeding its context window")]
    ContextLengthExceeded {
        /// The provider that refused.
        provider_id: String,
    },
    /// The request could not be built (serialization / configuration problem).
    /// This is a local programmer/config error, not a provider failure, so it
    /// has no [`FailureClass`].
    #[error("failed to build provider request: {0}")]
    Build(String),
    /// The egress choke point refused the call — a `local-only` boundary (BR-1)
    /// or the REQ-562 redaction scan. This is **not** a provider fault and is
    /// deliberately non-retryable: the authoritative `privacy_block` event
    /// already fired at the choke point, and the daemon must reroute the turn to
    /// the local tier rather than retry the blocked provider (REQ-544 M-1). It
    /// has no [`FailureClass`] because it is not a failure the
    /// retry/fallback/degrade machinery should ever act on.
    ///
    /// The [`BlockDetail`] rides through unchanged from the transport seam so
    /// the daemon can say which inspection refused the turn (REQ-562 BR-3).
    #[error("egress refused: {0}")]
    PrivacyBlocked(BlockDetail),
    /// The prompt reached its spend ceiling at the choke point (REQ-588 BR-3).
    ///
    /// Carries nothing: the sentence a user reads is composed where the facts
    /// are — the daemon holds the prompt's accumulator and the configured
    /// ceiling — so a payload here would be a second composer one layer below
    /// the one that can actually fill it in.
    #[error("this prompt reached its spend ceiling")]
    SpendCeilingReached,
}

impl ProviderError {
    /// Map to a [`FailureClass`], or `None` for the variants the retry /
    /// fallback / degrade machinery must not act on: [`ProviderError::Build`]
    /// (a local error), [`ProviderError::PrivacyBlocked`],
    /// [`ProviderError::EffortRefused`] and
    /// [`ProviderError::ContextLengthExceeded`] (typed outcomes the daemon
    /// handles out-of-band).
    #[must_use]
    pub fn failure_class(&self) -> Option<FailureClass> {
        Some(match self {
            ProviderError::Timeout => FailureClass::Timeout,
            ProviderError::Transport => FailureClass::Transport,
            ProviderError::ClientError { status } => FailureClass::ClientError { status: *status },
            ProviderError::ServerError { status } => FailureClass::ServerError { status: *status },
            ProviderError::MalformedResponse => FailureClass::MalformedResponse,
            ProviderError::MalformedToolCall { .. } => FailureClass::MalformedToolCall,
            // None of these is a failure the retry / fallback / degrade
            // machinery should act on. A privacy block is handled out-of-band
            // by rerouting to the local tier (REQ-544 M-1); an effort refusal
            // by a single retry with no reasoning field, remembered for the
            // session (REQ-559 BR-12 / ADR-F); a context-length refusal by a
            // typed report naming the window and the assembled size, with no
            // retry and no health change (REQ-586 BR-2 / ADR-8) — the same
            // bytes would overflow a fallback too. Handing any of them to
            // `classify` would degrade a provider that is working fine.
            // …and a spend-ceiling stop, for the same reason as the
            // context-length refusal beside it (REQ-588 BR-3 / ADR-4): the
            // provider is working fine, the *budget* ran out, and a fallback
            // would spend more money rather than less. Degrading it here would
            // make a budget decision look like an outage and route later turns
            // away from a healthy provider for the rest of the session.
            ProviderError::Build(_)
            | ProviderError::PrivacyBlocked(_)
            | ProviderError::EffortRefused { .. }
            | ProviderError::SpendCeilingReached
            | ProviderError::ContextLengthExceeded { .. } => return None,
        })
    }

    /// Whether this is the REQ-559 BR-12 effort refusal, which the daemon
    /// answers with exactly one retry carrying no reasoning field — never a
    /// silent retry of the same request, and never both shapes to see which
    /// works.
    #[must_use]
    pub const fn is_effort_refused(&self) -> bool {
        matches!(self, ProviderError::EffortRefused { .. })
    }

    /// Whether this is the REQ-586 BR-2 context-length refusal, which the
    /// daemon answers with a typed outcome naming the window and the assembled
    /// size — no retry, no failover, no change to the provider's health.
    #[must_use]
    pub const fn is_context_length_exceeded(&self) -> bool {
        matches!(self, ProviderError::ContextLengthExceeded { .. })
    }

    /// Whether this error is an egress privacy block (BR-1) — a distinct,
    /// non-retryable signal the daemon reroutes to the local tier rather than
    /// retrying the blocked provider (REQ-544 M-1).
    #[must_use]
    pub fn is_privacy_blocked(&self) -> bool {
        self.privacy_block_detail().is_some()
    }

    /// Which inspection refused the call, or `None` if this is not a privacy
    /// block (REQ-562 BR-3).
    ///
    /// [`Self::is_privacy_blocked`] is defined in terms of this rather than
    /// beside it, so "is it a block" and "which block is it" cannot come to
    /// disagree — a third refusal added to [`BlockDetail`] is answered by both
    /// at once.
    #[must_use]
    pub fn privacy_block_detail(&self) -> Option<BlockDetail> {
        match self {
            ProviderError::PrivacyBlocked(detail) => Some(*detail),
            _ => None,
        }
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
            // Preserve the privacy-block signal end to end: it must NOT collapse
            // into the retryable transport class (REQ-544 M-1).
            TransportError::PrivacyBlocked(detail) => ProviderError::PrivacyBlocked(detail),
            // Preserved end to end for the same reason (REQ-588 BR-3): folding
            // it into `Transport` would make it retryable and degrade a healthy
            // provider over a budget stop.
            TransportError::SpendCeiling => ProviderError::SpendCeilingReached,
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
        // REQ-586 BR-2: a context-length refusal is a typed outcome, not a
        // failure — the machinery must never retry, fail over or degrade on it.
        let too_long = ProviderError::ContextLengthExceeded {
            provider_id: "p".into(),
        };
        assert_eq!(too_long.failure_class(), None);
        assert_eq!(too_long.decision(), None);
        assert!(too_long.is_context_length_exceeded());
        assert!(!too_long.is_effort_refused());
        assert!(!ProviderError::Timeout.is_context_length_exceeded());
    }

    /// REQ-588 BR-3, the load-bearing leg: a spend-ceiling stop must leave
    /// provider health **unchanged**.
    ///
    /// This is asserted rather than assumed because the failure mode is quiet
    /// and long-lived. Degrading the provider here would make a budget decision
    /// look like an outage: the health tracker would mark a provider that is
    /// working perfectly, and later turns — turns the user has not even typed
    /// yet, possibly under a raised ceiling — would be routed away from it for
    /// the rest of the session. Nothing in the resulting behaviour would
    /// mention money, so nobody would connect the two.
    ///
    /// The `decision()` half is the second guarantee, and the one OQ-4 turned
    /// on: no `FailureAction::Fallback`. Falling back would reroute to another
    /// provider *because the budget ran out*, which spends more money rather
    /// than less — and does it silently, which is exactly the downgrade OQ-4
    /// rejected in favour of refusing and saying so.
    #[test]
    fn a_spend_ceiling_stop_is_not_a_provider_failure() {
        let stopped = ProviderError::SpendCeilingReached;
        assert_eq!(
            stopped.failure_class(),
            None,
            "the budget ran out; the provider is healthy and must not be marked"
        );
        assert_eq!(
            stopped.decision(),
            None,
            "no retry, no fallback, no degrade — a reroute would spend more, not less"
        );
        // It is its own outcome, not a borrowed one: the surfaces that special-
        // case a context-length refusal must not mistake this for one.
        assert!(!stopped.is_context_length_exceeded());
        assert!(!stopped.is_effort_refused());
    }

    // -----------------------------------------------------------------------
    // REQ-586 BR-2 / ADR-8: the context-length sniff
    // -----------------------------------------------------------------------

    /// A one-chunk error body, as the adapters hand it to the classifier.
    fn body_stream(body: &str) -> ByteStream {
        let chunks = vec![Ok::<Vec<u8>, TransportError>(body.as_bytes().to_vec())];
        Box::pin(futures::stream::iter(chunks))
    }

    /// The vendor spellings, each in the envelope its upstream actually
    /// sends: OpenAI's pretty-printed body (space after the colon), a compact
    /// compat body carrying only the code, a compat body carrying only the
    /// message (DeepSeek/vLLM wording), Anthropic's message, and
    /// `llama-server`'s — the local-engine class `budget::MIN_BUDGET_BYTES`
    /// names as the sub-floor case, whose body carries none of the four above.
    const CONTEXT_LENGTH_BODIES: [(&str, &str); 5] = [
        (
            "openai pretty-printed code",
            "{\n  \"error\": {\n    \"message\": \"This model's maximum context length is 128000 tokens. However, your messages resulted in 131000 tokens. Please reduce the length of the messages.\",\n    \"type\": \"invalid_request_error\",\n    \"param\": \"messages\",\n    \"code\": \"context_length_exceeded\"\n  }\n}",
        ),
        (
            "compat compact code only",
            r#"{"error":{"message":"Input too long for this endpoint","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#,
        ),
        (
            "compat message only",
            r#"{"error":{"message":"This model's maximum context length is 65536 tokens. However, you requested 70000 tokens (60000 in the messages, 10000 in the completion). Please reduce the length of the messages or completion.","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#,
        ),
        (
            "anthropic prompt is too long",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213717 tokens > 200000 maximum"},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#,
        ),
        (
            "llama.cpp exceed_context_size_error",
            r#"{"error":{"code":400,"message":"the request exceeds the available context size. try increasing the context size or enable context shift","type":"exceed_context_size_error","n_prompt_tokens":14429,"n_ctx":8192}}"#,
        ),
    ];

    /// The other `llama-server` build's wording, which differs in the trailing
    /// advice — pinned separately so the const stays the fragment both carry
    /// rather than drifting toward one build's whole sentence.
    #[test]
    fn both_observed_llama_server_wordings_name_context_length() {
        for body in [
            r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"server_error"}}"#,
            r#"{"error":{"code":400,"message":"the request exceeds the available context size. try increasing the context size or enable context shift","type":"exceed_context_size_error"}}"#,
        ] {
            assert!(body_names_context_length(body.as_bytes()), "{body}");
        }
    }

    #[test]
    fn each_vendor_spelling_names_context_length() {
        for (label, body) in CONTEXT_LENGTH_BODIES {
            assert!(body_names_context_length(body.as_bytes()), "{label}");
        }
    }

    /// The sniff is exact: a 400 that merely talks about size, tokens, or
    /// length in other words is **not** a context-length refusal, and neither
    /// is a non-UTF-8 body.
    #[test]
    fn resembling_a_size_complaint_is_not_enough() {
        let unrelated: [&str; 5] = [
            r#"{"error":{"message":"invalid model: no such model `test-model`"}}"#,
            r#"{"error":{"message":"max_tokens: 999999 > 8192, which is the maximum allowed for this model","type":"invalid_request_error"}}"#,
            r#"{"error":{"message":"Request too large for gpt-4o on tokens per min (TPM): Limit 30000, Requested 40000.","type":"tokens","code":"rate_limit_exceeded"}}"#,
            r#"{"error":{"message":"context length exceeded"}}"#,
            r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum allowed number of bytes."}}"#,
        ];
        for body in unrelated {
            assert!(!body_names_context_length(body.as_bytes()), "{body}");
        }
        assert!(!body_names_context_length(&[0xff, 0xfe, 0x22, 0x63]));
        assert!(!body_names_context_length(b""));
    }

    /// Every spelling classifies to the typed, class-less variant naming the
    /// provider — on a request that sent effort **and** on the `Omit` retry
    /// path, which is the one the effort sniff short-circuits and which ADR-8
    /// says must still read the head.
    #[test]
    fn a_400_with_a_vendor_spelling_is_the_typed_context_length_refusal() {
        let sent_shapes = [
            ResolvedEffort::effort(EffortLevel::High),
            ResolvedEffort::omit(EffortOmission::ShapeNone),
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        ];
        for (label, body) in CONTEXT_LENGTH_BODIES {
            for sent in sent_shapes {
                let err = futures::executor::block_on(classify_client_error(
                    400,
                    body_stream(body),
                    "p",
                    sent,
                ));
                assert_eq!(
                    err,
                    ProviderError::ContextLengthExceeded {
                        provider_id: "p".into()
                    },
                    "{label} / {sent:?}"
                );
                assert_eq!(err.failure_class(), None, "{label} / {sent:?}");
                assert_eq!(err.decision(), None, "{label} / {sent:?}");
                // No provider prose in the message (conventions.md).
                let msg = err.to_string();
                assert!(msg.contains("`p`"), "{msg}");
                assert!(
                    !msg.contains("maximum")
                        && !msg.contains("too long")
                        && !msg.contains("tokens"),
                    "no body text in the error: {msg}"
                );
            }
        }
    }

    /// An unrelated 400 takes today's path: `ClientError { 400 }`, classified
    /// `Fallback`. Adding the sniff must not move a single existing outcome.
    #[test]
    fn an_unrelated_400_still_classifies_fallback() {
        for sent in [
            ResolvedEffort::effort(EffortLevel::High),
            ResolvedEffort::omit(EffortOmission::ShapeNone),
        ] {
            let err = futures::executor::block_on(classify_client_error(
                400,
                body_stream(r#"{"error":{"message":"invalid model: no such model `test-model`"}}"#),
                "p",
                sent,
            ));
            assert_eq!(err, ProviderError::ClientError { status: 400 }, "{sent:?}");
            assert_eq!(
                err.decision().map(|d| d.action),
                Some(FailureAction::Fallback),
                "{sent:?}"
            );
        }
    }

    /// **Verify M3.** Both sniffs are 400-only: the *same* vendor spellings —
    /// context-length **and** effort — under a 401, a 403 or a 429 keep the
    /// classification they had before the sniffs existed.
    ///
    /// The two that matter are both in the loop. A 401/403 is a credential
    /// fault: `ClientError`, `FailureAction::Fail`, and the user is told to fix
    /// their key rather than to shrink a conversation that was never the
    /// problem. A 429 is a rate limit: `FailureAction::Retry`, so the backoff
    /// and the rate-limit degradation still happen. Both matter twice over
    /// because `ContextLengthExceeded` carries no [`FailureClass`] at all — an
    /// endpoint answering any 4xx with "prompt is too long" would otherwise keep
    /// itself in the route with its health untouched, and every later turn's
    /// full assembled context would keep going to it.
    ///
    /// Gateways (LiteLLM, OpenRouter, vLLM) relay an upstream vendor's error
    /// document under a status of their own, so these are bodies the adapters
    /// can really receive rather than a shape invented for a test.
    ///
    /// The **effort** leg is the same argument for the same shape, added a fix
    /// pass later: [`ProviderError::EffortRefused`] is likewise class-less, and
    /// it additionally writes the session's refusal memo — so a 401 whose body
    /// happens to say `reasoning_effort` would both immunize the endpoint and
    /// silently stop sending a level the provider accepts. REQ-559's spellings
    /// were every one of them read off a 400.
    #[test]
    fn no_status_but_400_can_be_a_typed_refusal() {
        /// Status, the action the pre-REQ-586 classification carries, a label.
        const NON_400: [(u16, FailureAction, &str); 4] = [
            (401, FailureAction::Fail, "a revoked or wrong key"),
            (403, FailureAction::Fail, "a key without access"),
            (429, FailureAction::Retry, "a rate limit"),
            (404, FailureAction::Fallback, "a missing route"),
        ];
        for (status, action, why) in NON_400 {
            for (label, body) in CONTEXT_LENGTH_BODIES {
                for sent in [
                    ResolvedEffort::effort(EffortLevel::High),
                    ResolvedEffort::omit(EffortOmission::ShapeNone),
                ] {
                    let err = futures::executor::block_on(classify_client_error(
                        status,
                        body_stream(body),
                        "p",
                        sent,
                    ));
                    assert_eq!(
                        err,
                        ProviderError::ClientError { status },
                        "{status} ({why}) with `{label}` in the body must stay a \
                         client error"
                    );
                    assert_eq!(
                        err.decision().map(|d| d.action),
                        Some(action),
                        "{status} ({why}) with `{label}` in the body lost its \
                         failure action"
                    );
                    assert!(
                        err.failure_class().is_some(),
                        "{status} ({why}) with `{label}` in the body stopped \
                         counting against the provider's health"
                    );
                    assert!(!err.is_context_length_exceeded(), "{status} / {label}");
                }
            }

            // …and the effort half, on a request that really did send a level:
            // the body names the field, the status says the request never got
            // as far as being read.
            let err = futures::executor::block_on(classify_client_error(
                status,
                body_stream(
                    r#"{"error":{"message":"Unrecognized request argument supplied: reasoning_effort"}}"#,
                ),
                "p",
                ResolvedEffort::clamped(EffortLevel::Xhigh, EffortLevel::High),
            ));
            assert_eq!(
                err,
                ProviderError::ClientError { status },
                "{status} ({why}) naming the reasoning field must stay a client \
                 error"
            );
            assert!(
                !err.is_effort_refused(),
                "{status} ({why}) must not poison the session's effort memo"
            );
            assert_eq!(
                err.decision().map(|d| d.action),
                Some(action),
                "{status} ({why}) naming the reasoning field lost its failure \
                 action"
            );
            assert!(
                err.failure_class().is_some(),
                "{status} ({why}) naming the reasoning field stopped counting \
                 against the provider's health"
            );
        }
    }

    /// The effort sniff keeps precedence: a body naming the reasoning field on
    /// a request that sent one is the effort refusal, even if it also mentions
    /// the context window — the more specific claim wins, and the daemon's
    /// `Omit` retry will surface the context-length refusal if it is real.
    #[test]
    fn the_effort_sniff_runs_before_the_context_length_sniff() {
        let body =
            r#"{"error":{"message":"reasoning_effort is not supported; prompt is too long"}}"#;
        let err = futures::executor::block_on(classify_client_error(
            400,
            body_stream(body),
            "p",
            ResolvedEffort::clamped(EffortLevel::Xhigh, EffortLevel::High),
        ));
        assert!(err.is_effort_refused(), "{err:?}");
        // And the same body on the Omit retry is the context-length refusal.
        let err = futures::executor::block_on(classify_client_error(
            400,
            body_stream(body),
            "p",
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        ));
        assert!(err.is_context_length_exceeded(), "{err:?}");
    }

    #[test]
    fn privacy_block_is_a_distinct_non_retryable_signal() {
        // REQ-544 M-1: a privacy block must NOT collapse into the retryable
        // transport class, and it carries no FailureClass (the daemon reroutes to
        // local out-of-band rather than retrying/falling back).
        let err =
            ProviderError::from_transport(TransportError::PrivacyBlocked(BlockDetail::Boundary));
        assert_eq!(err, ProviderError::PrivacyBlocked(BlockDetail::Boundary));
        assert!(err.is_privacy_blocked());
        assert_eq!(err.failure_class(), None);
        assert_eq!(err.decision(), None);
        // The other transport errors are unchanged.
        assert!(!ProviderError::from_transport(TransportError::Connect).is_privacy_blocked());
    }

    /// **REQ-562 BR-3.** Every detail survives the transport→provider hop
    /// unchanged, and each stays non-retryable.
    ///
    /// The mapping is a `match` over three values, which is exactly the shape
    /// that silently collapses when someone adds a fourth: this loop is what
    /// makes a collapsed arm fail rather than quietly report the wrong cause.
    #[test]
    fn every_block_detail_survives_the_hop_and_stays_non_retryable() {
        let details = [
            BlockDetail::Boundary,
            BlockDetail::Redaction,
            BlockDetail::ScanUnavailable,
        ];
        for detail in details {
            let err = ProviderError::from_transport(TransportError::PrivacyBlocked(detail));
            assert_eq!(
                err.privacy_block_detail(),
                Some(detail),
                "the cause must reach the daemon unchanged"
            );
            assert!(err.is_privacy_blocked());
            assert_eq!(err.failure_class(), None, "{detail:?}");
        }
        // Non-vacuity: a non-block error has no detail at all, so `Some(..)`
        // above is a fact about the variant rather than about the accessor.
        assert_eq!(ProviderError::Timeout.privacy_block_detail(), None);
    }

    /// The seam's own log clauses stay distinct, and the scan-unavailable one
    /// never reads as a finding (BR-3). No clause can carry payload content —
    /// [`BlockDetail`] has no field to carry it in.
    #[test]
    fn the_three_block_details_render_three_distinct_log_clauses() {
        let rendered: Vec<String> = [
            BlockDetail::Boundary,
            BlockDetail::Redaction,
            BlockDetail::ScanUnavailable,
        ]
        .into_iter()
        .map(|d| ProviderError::PrivacyBlocked(d).to_string())
        .collect();
        let unique: std::collections::BTreeSet<&String> = rendered.iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "the causes must not share a line: {rendered:?}"
        );

        assert!(
            rendered[0].contains("local-only privacy boundary"),
            "{rendered:?}"
        );
        assert!(
            rendered[1].contains("found sensitive content"),
            "{rendered:?}"
        );
        assert!(rendered[2].contains("could not run"), "{rendered:?}");
        assert!(
            !rendered[2].contains("found"),
            "a scan that never ran cannot have found anything: {rendered:?}"
        );
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
