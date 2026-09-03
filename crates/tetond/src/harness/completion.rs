//! The completion source: one abstraction the turn loop drives, over either the
//! **local** [`Engine`] tier or a **remote** [`Provider`].
//!
//! ## Why this exists (the TASK-010 integration gap)
//!
//! The turn loop ([`super::turn_loop`]) landed local-first: it called the local
//! [`Engine`] directly, so a phase routed to a remote model had nowhere to
//! actually run — the router picked a provider and built an egress context, but
//! nothing streamed a real (multi-turn, tool-using) session from it. This module
//! closes that gap. The loop no longer knows *what* produced a turn; it consumes a
//! [`CompletionSource`], and two implementations decide where the tokens come from:
//!
//! - [`LocalEngineSource`] — the offline AC-1 path, unchanged in spirit: lock the
//!   local engine, complete, parse the reply into a tool call or an end-of-turn.
//!   It takes no transport, so egress remains impossible on this path *by
//!   construction*. The completion itself runs on the blocking pool (E-3): a
//!   real llama.cpp turn takes seconds, and running it inline would park a
//!   tokio worker per session and stall every client's RPCs.
//! - [`RemoteProviderSource`] — drives a [`Provider`] through the single egress
//!   choke point ([`Egress`]). The provider only ever holds the provenance-scoped
//!   `&dyn Transport` egress hands it, so the same remote turn is subject to the
//!   privacy boundary (BR-1) and produces a `CostRecord` (BR-2) — the wiring the
//!   router already builds a context for, now actually executed.
//!
//! Both collapse a turn to the same [`SourceTurn`]: the assistant text and one
//! [`TurnDecision`] (call a tool, end the turn, or a malformed call folded back).
//! The loop switches on that and never sees a provider-specific shape.

use std::sync::{Arc, Mutex};
use teton_core::effort::{EffortOmission, ResolvedEffort};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use teton_inference::{ChatFormat, Completion, Engine, EngineError, MissReason};
use teton_protocol::events::{PrefixCache, PrefixCacheMiss, PrefixCacheOutcome};
use teton_protocol::{Category, Phase, ProviderId, SessionId};
use teton_providers::{
    Message, Provider, ProviderError, Role, TokenUsage, ToolSpec, Transport, TurnEvent, TurnRequest,
};

use crate::cost::{CostAttribution, LocalUsageMeter};
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::{
    approx_tokens, ContextManager, MessageRole, PreparedPrompt, Provenance as CtxProvenance,
    ToolProvenance,
};
use super::digest::tool_result_provenance;
use super::render;
use super::reply::{parse_reply, ParsedTurn, ReplyScanner};
use super::tools::ToolRegistry;
use super::turn_loop::{HarnessConfig, HarnessError};

/// What the model decided this turn — the single vocabulary the loop switches on,
/// regardless of whether a local engine or a remote provider produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnDecision {
    /// A well-formed call to a known tool.
    ToolCall {
        /// Tool name.
        name: String,
        /// Argument object.
        arguments: Value,
    },
    /// No tool call — the model's final answer for the turn.
    EndTurn {
        /// The plain-text answer.
        final_text: String,
    },
    /// Something tool-call-shaped but invalid (unknown tool, non-object args).
    /// Folded back to the model for correction, still under the turn ceiling.
    Malformed {
        /// Why the call was rejected (surfaced to the model).
        reason: String,
    },
}

/// One model turn produced by a [`CompletionSource`]: the assistant's text,
/// what it decided to do, and the token usage (populated for remote turns; the
/// local tier is free and reports zero).
#[derive(Debug, Clone)]
pub struct SourceTurn {
    /// The assistant's text for this turn (may be empty for a pure tool call).
    ///
    /// Already **cleaned** by the source (BUG-147): a local reply is cut at the
    /// end of its first tool call or at a fabricated transcript frame, so a
    /// weak model's hallucinated continuation never reaches context.
    pub text: String,
    /// The model's decision.
    pub decision: TurnDecision,
    /// Token usage, when the source knows it (remote). `0/0` for the local tier.
    pub usage: TokenUsage,
    /// Tool calls the model issued *beyond* the first this turn. The reduced
    /// harness runs one tool per turn; the loop folds a notice back so the
    /// model knows the rest did not run (BUG-147 — silently dropping them is
    /// what caused the re-emit loop).
    pub dropped_calls: u32,
    /// This turn's prefix-cache event payload (REQ-564), or `None` for a source
    /// that has no prefix cache — every remote turn.
    ///
    /// Carried on the turn rather than published by the source because the
    /// source has no event bus: the turn loop owns session-scoped emission, so
    /// it emits this, exactly once, from the async side.
    pub cache: Option<PrefixCache>,
    /// Whether a [`TurnDecision::ToolCall`] on this turn is **already embedded
    /// in [`text`](Self::text)** rather than arriving beside it (REQ-567 OQ-1).
    ///
    /// The answer follows **where the call came from**, and the difference is
    /// structural, not stylistic:
    ///
    /// - [`LocalEngineSource`] parses the call **out of the reply text**, so the
    ///   text literally ends with the call's JSON.
    /// - [`RemoteProviderSource`] usually receives the call as a structured
    ///   [`TurnEvent::ToolCall`]; then the text is prose only — often none at
    ///   all — and any JSON in it is something the model was *talking about*.
    ///   But a remote model may also *write* its call, in the text grammar the
    ///   system prompt teaches (BUG-180); with no native call on the turn the
    ///   source parses the text exactly as the local tier does, and then the
    ///   text ends with the call just as a local reply's does.
    ///
    /// Either way the block the loop pushes **ends with the call**: the loop
    /// renders a native remote call onto the prose in the reply grammar before
    /// it pushes (BUG-178 — an assistant turn pushed as the bare prose was empty
    /// whenever the model said nothing first, and every remote provider refuses
    /// an empty assistant turn on the next request). This flag says who put the
    /// call there, so the loop knows whether it still has to; the trailing
    /// position is what lets OQ-1's cancellation trim cut the call — and only
    /// the call — back out of prose that may itself quote something call-shaped
    /// like `{"name": "serde", "version": "1"}`.
    ///
    /// Always `false` when the decision is not a tool call — there is no call to
    /// be embedded.
    pub call_in_text: bool,
}

/// A source of model turns for the turn loop: local engine or remote provider.
///
/// `produce_turn` is handed the already-assembled [`PreparedPrompt`] (both the
/// flat string a local text engine consumes and the system-prompt + role-typed
/// messages a remote chat provider consumes, REQ-544 M-8), the egress
/// [`Provenance`] of the context it was assembled from (BR-1; ignored by the
/// local path), the harness `config`, the tool set, the exposed tool names, and an
/// `on_token` sink for streaming. It returns exactly one [`SourceTurn`]. Bound
/// `Send` so the daemon can drive a turn from any task.
#[async_trait]
pub trait CompletionSource: Send {
    /// Produce one model turn for `prompt`.
    ///
    /// # Errors
    /// [`HarnessError::Engine`] for a local backend failure, or
    /// [`HarnessError::Remote`] for a provider/transport failure (a privacy block
    /// surfaces here as a transport-level refusal — see [`RemoteProviderSource`]).
    async fn produce_turn(
        &mut self,
        prompt: &PreparedPrompt,
        provenance: &Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError>;

    /// The prompt rendering the model behind this source is actually shown
    /// (REQ-554 ADR-4).
    ///
    /// The turn loop reads it to build a [`StreamGate`](super::reply::StreamGate)
    /// whose fabrication markers match the delimiters on screen. Mismatched
    /// markers fail in both directions: flat markers under a templated prompt cut
    /// a legitimate answer that happens to start a line with `User:`, and missing
    /// template markers under a templated prompt let a fabricated
    /// `<|im_start|>user` turn stream straight to the user.
    ///
    /// Defaults to [`ChatFormat::Flat`], which is right for every source that is
    /// not a local text engine. [`RemoteProviderSource`] stays `Flat` on purpose:
    /// fabrication markers are a *transcript rendering* concern — a remote
    /// provider is shown role-typed messages and owns its own wire format, so
    /// there is no template frame in its text for a ChatML marker to describe.
    /// (Its text may still carry a tool call in the reply grammar, BUG-180, and
    /// the flat gate already holds that back — the grammar is format-agnostic.)
    /// The default also keeps every test source compiling unchanged.
    fn chat_format(&self) -> ChatFormat {
        ChatFormat::Flat
    }
}

/// The size of what an attempt actually sent, in the harness's own word
/// estimator (REQ-586 BR-2).
///
/// Measured off the **prepared prompt** rather than off the [`ContextManager`],
/// for two reasons. It is what a request is built from — system field plus the
/// role-typed messages, which is exactly the payload the provider counted — and
/// it is what this frame can reach: `produce_turn` is handed a prompt, never the
/// manager that assembled it, and threading one in to read a number would hand
/// every completion source a mutable handle on the conversation to serve an
/// error path.
///
/// The same [`approx_tokens`] the budget gate measures with, so the figure this
/// reports and the budget it is reported against are in one currency — a report
/// mixing a word estimate with a BPE count would make every refusal look like a
/// wildly wrong window.
///
/// Free-standing rather than a method on one source (REQ-589 ADR-3): both tiers
/// now report a window refusal, and two copies of this sum are two figures that
/// can drift apart while claiming the same currency.
fn assembled_words(prompt: &PreparedPrompt) -> usize {
    approx_tokens(&prompt.system)
        + prompt
            .messages
            .iter()
            .map(|m| approx_tokens(&m.text))
            .sum::<usize>()
}

/// The local tier's failure, classified (REQ-589 ADR-3) — **the one home** of
/// the mapping, the local twin of
/// [`RemoteProviderSource::context_length_exceeded`].
///
/// The engine has two ways to refuse a turn and they want opposite handling: an
/// inference failure is an internal error the daemon can only report, while a
/// prompt that does not fit the window is a *context outcome* with remedies —
/// the backstop BR-3, BR-12 and BR-14.1 all name, on the tier the reported
/// `/analyze` failure actually ran on.
///
/// Told apart by [`EngineError`]'s **variant**, never by its sentence. Matching
/// the message would be a predicate mirrored away from the precondition that
/// produced it (LESSON-528): the wording lives in `teton_inference::over_window`,
/// a reword there would silently reclassify every local window refusal back to
/// an internal error, and nothing in this crate would redden.
///
/// The engine's own token counts are deliberately dropped. They are measured in
/// a different currency (real BPE tokens against the engine's `n_ctx`) than the
/// route's word budget, and a sentence pairing the two would make every refusal
/// look like a wildly wrong window. What the report needs — the harness's
/// estimate and the harness's budget — is in scope right here.
fn classify_engine_failure(
    err: EngineError,
    prompt: &PreparedPrompt,
    config: &HarnessConfig,
) -> HarnessError {
    match err {
        EngineError::ContextWindowExceeded { .. } => HarnessError::LocalContextLengthExceeded {
            assembled_tokens: assembled_words(prompt),
            budget_tokens: config.context_budget_tokens,
        },
        other => HarnessError::Engine(other),
    }
}

/// The local-tier source: drives the [`Engine`] behind a shared `Arc<Mutex<_>>`
/// and parses its text reply. Transport-free — egress is impossible on this path.
///
/// The source holds an *owned* handle (not a borrow) because the completion runs
/// inside [`tokio::task::spawn_blocking`], whose closure must be `'static`.
pub struct LocalEngineSource {
    engine: Arc<Mutex<dyn Engine>>,
    /// The rendering the loaded engine speaks (REQ-554 ADR-2), read once at
    /// construction. **Immutable for this source's life**: the format is a
    /// property of the committed engine, resolved at load time from its GGUF
    /// template, and a committed engine is never re-templated.
    format: ChatFormat,
    /// Where completed local turns are recorded (REQ-564 BR-9), or `None` for
    /// a source with no ledger — the test entry points, and any caller that
    /// only wants the turn.
    ///
    /// The remote tier meters itself at the egress choke point every remote
    /// call flows through. The local tier is transport-free, so it has no such
    /// seam and carries its own sink instead.
    meter: Option<Arc<dyn LocalUsageMeter>>,
    /// The session this source's turns belong to — the prefix cache's key
    /// (REQ-564 BR-3).
    ///
    /// A **parameter for the same reason `format` is one**: reading it off the
    /// engine would need the mutex, and that mutex is held for the whole of any
    /// in-flight completion (LESSON-448). Only agent turns carry a key; duties
    /// go through [`Engine::complete`], which has no way to name a session, so
    /// a duty cannot evict this session's prefix (BR-5).
    session_id: SessionId,
}

impl LocalEngineSource {
    /// A source over the shared local `engine`, rendering for `format`.
    ///
    /// The format is a **parameter, not a lock**: this constructor runs on the
    /// async path (`run_session_turn`, the routed attempt), and the engine
    /// mutex is held for the entire seconds-long duration of any in-flight
    /// completion — locking here to ask the engine its format would park a
    /// tokio worker behind another session's inference (LESSON-448; REQ-554
    /// verify). The caller supplies the format resolved when the engine was
    /// installed (the daemon's engine slot stores it beside the handle), which
    /// cannot disagree with the engine: the slot replaces handle and format
    /// together, and a committed engine is never re-templated (ADR-2).
    #[must_use]
    pub fn new(engine: Arc<Mutex<dyn Engine>>, format: ChatFormat, session_id: SessionId) -> Self {
        Self {
            engine,
            format,
            meter: None,
            session_id,
        }
    }

    /// Record this source's completed turns into `meter` (REQ-564 BR-9).
    ///
    /// Opt-in rather than required so the harness-only entry points, which have
    /// no ledger, stay constructible with no change.
    #[must_use]
    pub fn metered(mut self, meter: Arc<dyn LocalUsageMeter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// This turn's prefix-cache outcome, in wire form.
    ///
    /// Derived from the completion the engine returned, so the event and the
    /// ledger row can never disagree about what happened — both read this one
    /// projection rather than each recomputing it.
    ///
    /// An engine **error** never reaches here: it is an `Err` from `complete_*`
    /// and never becomes a miss (BR-8).
    fn cache_outcome(completion: &Completion) -> PrefixCacheOutcome {
        match completion.cache_miss {
            None => PrefixCacheOutcome::Hit {
                cached_tokens: u64::from(completion.cached_tokens),
                new_tokens: u64::from(completion.processed_tokens()),
                divergent: completion.cache_divergent,
            },
            Some(reason) => PrefixCacheOutcome::Miss {
                reason: match reason {
                    MissReason::Cold => PrefixCacheMiss::Cold,
                    MissReason::SessionSwitch => PrefixCacheMiss::SessionSwitch,
                    MissReason::Divergent => PrefixCacheMiss::Divergent,
                    MissReason::Evicted => PrefixCacheMiss::Evicted,
                },
                processed_tokens: u64::from(completion.processed_tokens()),
            },
        }
    }
}

#[async_trait]
impl CompletionSource for LocalEngineSource {
    fn chat_format(&self) -> ChatFormat {
        self.format
    }

    async fn produce_turn(
        &mut self,
        prompt: &PreparedPrompt,
        _provenance: &Provenance,
        config: &HarnessConfig,
        _tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError> {
        // REQ-554 BR-1: render the *already role-typed* prompt for the format
        // the loaded model actually speaks — its native ChatML template when the
        // GGUF carries one, the flat transcript (byte-identical to before) when
        // it does not. This rendered string is what `complete` tokenizes, so the
        // engine's typed over-window refusal inherently measures the prompt
        // including template overhead (BR-5).
        //
        // Real inference rides the blocking pool (E-3): a llama.cpp completion
        // holds a core for seconds, so run inline it would park this tokio
        // worker — one slow local turn per worker and the whole daemon stops
        // answering RPCs. The engine handle is moved into `spawn_blocking`, and
        // its token stream is bridged back over a channel so the caller's
        // `on_token` sink still observes tokens (and first-token latency) live.
        let engine = Arc::clone(&self.engine);
        let rendered = render::render_prompt(self.format, prompt);
        let format = self.format;
        let params = config.gen_params;
        let session = self.session_id.0.clone();
        let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let task = tokio::task::spawn_blocking(move || {
            // BUG-147: the scanner ends the turn at the first complete tool
            // call or at a fabricated transcript frame — a weak model left to
            // run would go on to invent tool results and future turns (and burn
            // seconds of inference producing them). Its marker set follows the
            // rendering (BR-4): the frames the model can fabricate are the ones
            // it was just shown.
            let mut scanner = ReplyScanner::for_format(format);
            let mut guard = engine.lock().expect("engine mutex poisoned");
            // Read from the guard already held here, inside the blocking task:
            // taking a second lock on the async path to ask the engine its model
            // id would park a tokio worker behind whatever completion currently
            // owns the mutex (LESSON-448) — the same reason `duty.rs` reads
            // `chat_format` here rather than outside.
            let model = guard.model_id().to_owned();
            // REQ-564: the agent turn — and only the agent turn — asks for
            // prefix reuse. `complete_cached` defaults to a cold `complete`, so
            // an engine with no cache behaves exactly as it did before.
            let completion = guard.complete_cached(&session, &rendered, &params, &mut |token| {
                // A closed receiver means the caller went away; keep completing
                // (spawn_blocking is not cancellable) and drop the token.
                let _ = token_tx.send(token.to_owned());
                scanner.push(token)
            });
            completion.map(|completion| (model, completion))
        });
        while let Some(token) = token_rx.recv().await {
            on_token(&token);
        }
        // The sender is dropped when the closure returns, ending the loop above,
        // so this join is immediate. A panicked/aborted task is a backend
        // failure, not a daemon crash.
        //
        // REQ-589 ADR-3: the engine's own failure is classified rather than
        // blanket-wrapped in `HarnessError::Engine` — a window refusal leaves
        // here as the typed context outcome so the daemon can name a remedy
        // instead of reporting an internal error. A join failure is not the
        // engine answering about its window, so it keeps the backend shape it
        // has always had and takes the `other` arm.
        let completed = task.await.unwrap_or_else(|_| {
            Err(EngineError::Backend(
                "the local inference task did not complete".to_owned(),
            ))
        });
        let (model, completion) =
            completed.map_err(|err| classify_engine_failure(err, prompt, config))?;
        // Projected before `completion.text` is moved out, so the event
        // describes the completion the engine actually returned.
        let cache = PrefixCache {
            model,
            outcome: Self::cache_outcome(&completion),
        };
        // One usage row per completed local turn (BR-9). `input_tokens` stays
        // the full prompt; `cached_tokens` says how much of it came for free.
        if let Some(meter) = self.meter.as_ref() {
            meter.local_call(
                &self.session_id,
                &CostAttribution::new(cache.model.clone()),
                u64::from(completion.prompt_tokens),
                u64::from(completion.completion_tokens),
                u64::from(completion.cached_tokens),
            );
        }
        // Cut the reply at the turn boundary (re-scanned over the final text —
        // deterministic, and independent of how the stream was chunked), then
        // parse the *clean* reply. Everything past the first tool call — the
        // hallucinated continuation — never reaches context.
        let mut text = completion.text;
        text.truncate(ReplyScanner::scan_all_for(self.format, &text).context_cut());
        let parsed = parse_reply(&text, exposed);
        let decision = match parsed.turn {
            ParsedTurn::ToolCall { name, arguments } => TurnDecision::ToolCall { name, arguments },
            ParsedTurn::EndTurn(final_text) => TurnDecision::EndTurn { final_text },
            ParsedTurn::Malformed(reason) => TurnDecision::Malformed { reason },
        };
        text.truncate(parsed.clean_len);
        // REQ-567 OQ-1: this tier's call *is* the text — `parse_reply` found it
        // there and `clean_len` keeps it — so the block the loop pushes carries
        // it and the cancellation trim has something to cut.
        let call_in_text = matches!(decision, TurnDecision::ToolCall { .. });
        Ok(SourceTurn {
            text,
            decision,
            usage: TokenUsage {
                input_tokens: u64::from(completion.prompt_tokens),
                output_tokens: u64::from(completion.completion_tokens),
                // The local engine reports no reasoning split — thinking there
                // is a chat-template property, not a counted category (BR-6).
                reasoning_tokens: None,
            },
            dropped_calls: parsed.dropped_calls,
            cache: Some(cache),
            call_in_text,
        })
    }
}

/// The remote-tier source: drives a [`Provider`] through the single egress choke
/// point.
///
/// The provider is handed only the provenance-scoped `&dyn Transport` that
/// [`Egress::scoped`] produces, so every byte it sends is inspected against the
/// privacy boundaries (BR-1) and, on an allowed forward, metered into one
/// `CostRecord` (BR-2) — exactly the guarantees the router's egress context was
/// built for. A privacy block manifests as a transport refusal
/// ([`ProviderError::Transport`]); the authoritative `privacy_block` event has
/// already been emitted at the choke point.
pub struct RemoteProviderSource<'a, T: Transport> {
    provider: &'a dyn Provider,
    egress: &'a Egress<T>,
    provider_id: ProviderId,
    model: String,
    session_id: SessionId,
    phase: Option<Phase>,
    category: Option<Category>,
    /// What this source's requests put in their reasoning field(s) (REQ-559).
    ///
    /// A **required** constructor argument, not a builder with a default: BR-1
    /// says omitting effort is never a valid outcome, and a defaulting builder
    /// would let a call site forget silently. The value is resolved once at
    /// route time (ADR-G) and carried here unchanged — this source never clamps.
    effort: ResolvedEffort,
    /// Set when this provider refused the effort field on this turn (REQ-559
    /// BR-12 / ADR-F).
    ///
    /// Read by the daemon after the turn to populate the **session** refusal
    /// memo, so the next call to this provider does not repeat a request already
    /// known to fail. Never persisted, and it does not touch the declared
    /// `reasoning_shape`: BR-4 forbids sniffing a shape from a response, so this
    /// is a runtime degradation record and not a capability conclusion.
    effort_refused: bool,
}

impl<'a, T: Transport> RemoteProviderSource<'a, T> {
    /// A source that drives `provider` for `session_id`, billing `model` under
    /// `provider_id`, with every call routed through `egress`.
    pub fn new(
        provider: &'a dyn Provider,
        egress: &'a Egress<T>,
        provider_id: impl Into<ProviderId>,
        model: impl Into<String>,
        session_id: impl Into<SessionId>,
        effort: ResolvedEffort,
    ) -> Self {
        Self {
            provider,
            egress,
            provider_id: provider_id.into(),
            model: model.into(),
            session_id: session_id.into(),
            phase: None,
            category: None,
            effort,
            effort_refused: false,
        }
    }

    /// Whether this source's provider refused the effort field (REQ-559 BR-12).
    ///
    /// The daemon reads this after the turn to seed the session memo (ADR-F).
    #[must_use]
    pub fn effort_was_refused(&self) -> bool {
        self.effort_refused
    }

    /// Pin the structured-mode `phase` this source's calls are attributed to
    /// (drives per-phase cost attribution, AC-3/BR-2). Absent in freeform mode.
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Pin the routing `category` this source's calls were made **for**
    /// (REQ-558 BR-11).
    ///
    /// The other half of `with_phase`, and independent of it: the phase is what
    /// the spend is *attributed* to, the category is what it was *for*. A
    /// freeform session has the second without the first, and a structured one
    /// has both on the same row — which is the point, because "what did `edit`
    /// cost me across every session" is a question the phase column cannot
    /// answer.
    ///
    /// The caller passes the category the routing decision **resolved**, read
    /// off `Route::resolution`, never one re-derived from the phase (ADR-D).
    #[must_use]
    pub fn with_category(mut self, category: Category) -> Self {
        self.category = Some(category);
        self
    }

    /// The typed report for a provider that refused the request as too big for
    /// its window (REQ-586 BR-2, ADR-8) — **the one home** of the mapping.
    ///
    /// Written once because there are two ways into it and they must not
    /// diverge: the first attempt, and the REQ-559 BR-12 retry with no
    /// reasoning field. The retry used to propagate with `?`, and
    /// [`HarnessError::Remote`] is `#[from] ProviderError`, so a
    /// context-length refusal on *that* call became `Remote(..)` — a
    /// class-less error the runtime's retry arm cannot act on, which the user
    /// saw as `INTERNAL_ERROR "provider failed unrecoverably"` instead of the
    /// window/size report the same refusal produces one line earlier (verify
    /// M2). The prompt is too big for the window whether or not the request
    /// carried an effort field.
    fn context_length_exceeded(
        &self,
        prompt: &PreparedPrompt,
        config: &HarnessConfig,
    ) -> HarnessError {
        HarnessError::ContextLengthExceeded {
            provider_id: self.provider_id.to_string(),
            assembled_tokens: assembled_words(prompt),
            budget_tokens: config.context_budget_tokens,
        }
    }

    /// Say on the daemon's stderr that this provider failed the turn, and how.
    ///
    /// BUG-178 was diagnosed by replaying the request by hand, because the one
    /// thing that would have named it — the provider answering HTTP 400 to the
    /// request Teton built — reached the user only as `invalid response` and
    /// reached the daemon log not at all. This line is that fact, next to the
    /// provider and the moment (`when`): "before it answered" is the request
    /// itself being refused, "mid-stream" is a body that stopped parsing.
    ///
    /// Provider-side failures only — the ones that carry a
    /// [`FailureClass`](teton_providers::FailureClass). A privacy block, an
    /// effort refusal and a build error already announce themselves on their
    /// own paths and would only be repeated here. Content-free by construction:
    /// [`ProviderError`]'s `Display` is a class and a status, never a body,
    /// prompt text, or path (BR-11).
    fn note_failure(&self, when: &str, err: &ProviderError) {
        if err.failure_class().is_some() {
            eprintln!(
                "teton: provider `{}` failed the turn {when}: {err}",
                self.provider_id
            );
        }
    }
}

#[async_trait]
impl<T: Transport> CompletionSource for RemoteProviderSource<'_, T> {
    async fn produce_turn(
        &mut self,
        prompt: &PreparedPrompt,
        provenance: &Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError> {
        // REQ-544 M-8: map the structured context to a real system prompt plus
        // role-typed user/assistant messages — NOT one collapsed `Role::User`
        // blob. Preserving the system field and the assistant turns keeps
        // tool-calling fidelity on weak providers and lets prompt caching hit.
        let messages = prompt
            .messages
            .iter()
            .map(|m| Message {
                role: match m.role {
                    MessageRole::User => Role::User,
                    MessageRole::Assistant => Role::Assistant,
                },
                content: m.text.clone(),
            })
            .collect();
        let system = if prompt.system.trim().is_empty() {
            None
        } else {
            Some(prompt.system.clone())
        };
        let request = TurnRequest {
            model: self.model.clone(),
            system,
            messages,
            tools: exposed_tool_specs(tools, config.max_tools),
            max_tokens: config.gen_params.max_tokens,
            // REQ-559 BR-1: resolved once at route time and carried here. This
            // site does not clamp, does not default, and cannot omit — the
            // field is required and `ResolvedEffort` has no `Default` (ADR-B).
            effort: self.effort,
        };

        // BR-2 / REQ-558 BR-11: attribute the call to (session, phase, category,
        // model). Both dimensions travel, and neither is derived from the other:
        // the phase says what the spend belongs to, the category says what it
        // bought. BR-1: the scoped transport bakes in this turn's provenance so
        // the provider cannot bypass the boundary check.
        let mut attribution = CostAttribution::new(self.model.clone());
        if let Some(phase) = self.phase {
            attribution = attribution.with_phase(phase);
        }
        if let Some(category) = self.category {
            attribution = attribution.with_category(category);
        }
        // Built as a closure rather than a value because the BR-12 fallback
        // below needs a second one: an `EgressContext` is consumed by `scoped`,
        // and the retry is a distinct call through the same choke point, so it
        // must be boundary-checked and metered on its own terms — never handed a
        // reused context that would fold two calls into one CostRecord.
        let egress_ctx = || {
            EgressContext::new(self.provider_id.clone())
                .with_session(self.session_id.clone())
                .with_cost(attribution.clone())
        };
        let transport = self.egress.scoped(provenance.clone(), egress_ctx());

        // Errors known at open time (including a privacy block, surfaced as a
        // transport refusal) come back here before any events flow.
        //
        // REQ-559 BR-12: a provider that refuses the reasoning-effort field gets
        // **exactly one** retry, with no reasoning field at all. Not a silent
        // retry of the same request — that is what BR-12 forbids — and never
        // both shapes "to see which works". The retry cannot loop by
        // construction: it sends `Omit`, and `classify_client_error`
        // short-circuits on a request that carried no reasoning field, so a
        // second refusal is impossible rather than merely unlikely.
        // Cloned only when a refusal is *possible* — i.e. when this request
        // actually carries an effort field. A turn whose resolution is `Omit` or
        // `ThinkingFlag` can never come back as an effort refusal
        // (`classify_client_error` short-circuits), so it must not pay to copy a
        // whole conversation on the hot path for a fallback it cannot take.
        let retry_seed =
            matches!(request.effort, ResolvedEffort::Effort { .. }).then(|| request.clone());
        let mut stream = match self.provider.stream_turn(request, &transport).await {
            Ok(stream) => stream,
            Err(err) if err.is_effort_refused() => {
                // Loud, not silent (LESSON-447): the typed error names the
                // provider and both levels, and the session surface reports the
                // provider as refusing rather than showing a level it is not
                // receiving (BR-6).
                eprintln!("teton: {err}; retrying this call with no reasoning field");
                self.effort_refused = true;
                let seed = retry_seed.expect(
                    "a refusal is only classified for a request that carried an \
                     effort field, which is exactly when the seed was taken",
                );
                let fallback = TurnRequest {
                    effort: ResolvedEffort::Omit {
                        reason: EffortOmission::RefusedThisSession,
                    },
                    ..seed
                };
                // A fresh scoped transport: the first attempt's was consumed,
                // and this is a second call through the same choke point, so it
                // is boundary-checked and metered on its own terms (BR-13 —
                // effort changes nothing about egress).
                let transport = self.egress.scoped(provenance.clone(), egress_ctx());
                // Matched, never `?`-ed: the arm below this one types a
                // context-length refusal, and `?` would route the *retry's*
                // identical refusal through `#[from] ProviderError` into
                // `Remote(..)` — an error with no failure class, which the
                // runtime cannot retry, cannot fail over, and reports as an
                // opaque internal error (verify M2).
                match self.provider.stream_turn(fallback, &transport).await {
                    Ok(stream) => stream,
                    Err(err) if err.is_context_length_exceeded() => {
                        return Err(self.context_length_exceeded(prompt, config));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            // REQ-586 BR-2 / ADR-8: the request was too big for the window.
            // Typed here, at the seam that holds both halves of the report —
            // the adapter knows only that the provider refused, and the loop
            // above knows only that a turn ended, but *this* frame has the
            // assembled prompt and the route's budget side by side.
            //
            // No `note_failure`, and not because the call would print nothing
            // (a class-less error is already filtered there): the line's whole
            // subject is "provider `x` failed the turn", and this provider did
            // not fail. The daemon reports the size mismatch instead.
            //
            // Open time only, mirroring the effort refusal beside it: a
            // context-length refusal is a 400 on the request, which is answered
            // before a single event flows. A mid-stream error is a different
            // fault and keeps the `Remote` path it has today.
            Err(err) if err.is_context_length_exceeded() => {
                return Err(self.context_length_exceeded(prompt, config));
            }
            Err(err) => {
                self.note_failure("before it answered", &err);
                return Err(err.into());
            }
        };

        let mut text = String::new();
        let mut tool_call: Option<TurnDecision> = None;
        let mut dropped_calls = 0u32;
        let mut usage = TokenUsage::default();
        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    self.note_failure("mid-stream", &err);
                    return Err(err.into());
                }
            };
            match event {
                TurnEvent::TextDelta(delta) => {
                    on_token(&delta);
                    text.push_str(&delta);
                }
                // MVP: the reduced harness runs one tool per turn, so the first
                // assembled call wins; later parallel calls this turn are
                // counted so the loop can tell the model they did not run
                // (BUG-147 — a silent drop makes the model re-emit them).
                TurnEvent::ToolCall(call) if tool_call.is_none() => {
                    tool_call = Some(TurnDecision::ToolCall {
                        name: call.name,
                        arguments: call.arguments,
                    });
                }
                TurnEvent::ToolCall(_) => dropped_calls += 1,
                TurnEvent::Completed(completion) => {
                    usage = completion.usage;
                }
            }
        }

        // BUG-180: a provider that sent no native call may still have *called*
        // — in the text grammar the system prompt teaches every model and the
        // carried history shows it on every prior call (`append_tool_call`,
        // BUG-178). That text is read by the same `parse_reply` the local tier
        // uses, so a call the loop dispatches is recognized by one grammar
        // whichever source produced it (LESSON-494). Left unparsed, the call
        // was the turn's *answer*: the gate hid it as tool-shaped, nothing ran,
        // nothing rendered, and the bare JSON was committed as the assistant's
        // reply — a silent empty turn.
        //
        // A native call still wins outright and leaves the text alone: on that
        // path the prose is prose, however call-shaped some JSON in it may
        // look (REQ-567 OQ-1), and the loop renders the call onto it. Only the
        // *absence* of a native call makes the text the place to look.
        let (decision, call_in_text, dropped_calls) = match tool_call {
            Some(call) => (call, false, dropped_calls),
            None => {
                let parsed = parse_reply(&text, exposed);
                let decision = match parsed.turn {
                    ParsedTurn::ToolCall { name, arguments } => {
                        TurnDecision::ToolCall { name, arguments }
                    }
                    ParsedTurn::EndTurn(final_text) => TurnDecision::EndTurn { final_text },
                    // An unknown tool or non-object arguments: folded back as
                    // the correction the local tier gets, never accepted as an
                    // answer.
                    ParsedTurn::Malformed(reason) => TurnDecision::Malformed { reason },
                };
                // The call *is* the text: keep through its end, drop whatever
                // the model went on to write past it, and say so with
                // `call_in_text` so the loop does not render the call twice.
                text.truncate(parsed.clean_len);
                let call_in_text = matches!(decision, TurnDecision::ToolCall { .. });
                (decision, call_in_text, parsed.dropped_calls)
            }
        };
        Ok(SourceTurn {
            text,
            decision,
            usage,
            dropped_calls,
            // A remote provider has no local KV to reuse; `None` says "this
            // source has no prefix cache", which is a different fact from a
            // miss and must not be reported as one.
            cache: None,
            // REQ-567 OQ-1 / BUG-180: `true` only for a text-form call this
            // source parsed out of the prose — the call is already at the
            // text's end, where `clean_len` left it. A native call arrives as a
            // structured `TurnEvent::ToolCall` beside prose that does not
            // contain it, so the loop renders it on before the block is pushed
            // (BUG-178) and this stays `false`: the guarantee that a tool-call
            // block carries its call belongs at the one seam every source
            // passes through, and this flag only says who put it there.
            call_in_text,
        })
    }
}

/// The egress [`Provenance`] of the context currently assembled in `ctx`: the
/// union of every block that carries file provenance.
///
/// This is the loop → egress bridge for BR-1 (REQ-544 C-1). A tool result tagged
/// with the files it touched contributes those paths; a result with UNKNOWN
/// provenance (a `shell` command) makes the whole context's provenance unknown,
/// which egress fail-closes. The remote source hands the result to
/// [`Egress::scoped`], so a turn whose context touched a `local-only` file — or
/// ran an unparseable shell command — is blocked before a byte leaves.
///
/// ## A user block can carry file provenance too (REQ-585 BR-7)
///
/// System and model blocks carry none, and user blocks used to be in that list.
/// They are not any more: a `/skill` invocation expands a `SKILL.md` into the
/// user turn, so that turn is prompt text by role and *file content* by origin
/// and has to pin the turn exactly as a `read` of the same file would. Its
/// sources fold in through the identical mapping the tool results use, and a
/// skill whose file has no mintable identity — a user-scoped skill outside the
/// session root — folds in as `Unknown` and fail-closes, the strictest reading
/// and the one that keeps a file outside the root from silently counting as
/// unpinnable (ADR-9).
///
/// ## Forgotten blocks are counted too (REQ-567 BR-3)
///
/// The union is over the blocks the context *holds* **plus** the
/// [`DroppedProvenance`](super::context::DroppedProvenance) of the ones
/// `truncate_to_budget` took away. A dropped block's content routinely outlives
/// it — in the model text right after it, in a compaction summary, in the next
/// prompt's carried conversation — and a scope computed from surviving blocks
/// alone would call that content ordinary conversation and let it egress. So a
/// `local-only` read that has since been truncated away still scopes this
/// context, and an unknown-provenance `shell` result still fail-closes it.
///
/// ## The system prompt can carry file provenance too (REQ-612 BR-5)
///
/// `System` blocks still contribute nothing — the prompt's harness prose is
/// this build's own text — but since REQ-612 the prompt's **tail** can be a
/// repository's `TETON.md`, and that file's identity is on the manager
/// ([`ContextManager::system_sources`](super::context::ContextManager::system_sources)),
/// not on a block. It folds in through the same mapping, so a `local-only`
/// boundary covering the notes scopes the turn exactly as a `read` of them
/// would. Without this union the notes would reach every remote provider on
/// every turn with no boundary verdict at all — the one path around the
/// charter's BR-1, and the reason ADR-2 puts the set on the manager rather than
/// leaving the load-time check to stand alone.
#[must_use]
pub fn context_provenance(ctx: &ContextManager) -> Provenance {
    let mut prov = Provenance::empty();
    // Through that same mapping again (REQ-612 BR-5, ADR-2): a repository file
    // in the **system prompt** means to egress exactly what a `read` of it
    // means. `System => {}` below is still right for the prompt's harness prose;
    // what changed is that the prompt can now carry file bytes, and the
    // identity of those bytes rides on the manager rather than on a block —
    // see `ContextManager::system_sources` for why it cannot be a block.
    //
    // First, because the system prompt is the first thing the provider reads,
    // and because a union is a union: nothing here depends on the order, and a
    // reader looking for "where do the prompt's own files fold in" finds it
    // before the loop rather than after it.
    let system_sources = ctx.system_sources();
    if !system_sources.is_empty() {
        prov.merge(&tool_result_provenance(&ToolProvenance::Sources(
            system_sources.clone(),
        )));
    }
    for block in ctx.blocks() {
        match &block.provenance {
            CtxProvenance::Tool { provenance, .. } => {
                // One per-result mapping, shared with the `digest` duty (which
                // scopes a *single* result rather than the whole context). Two
                // spellings of "what does this tool result mean to egress" is
                // how one of them ends up laxer than the other.
                prov.merge(&tool_result_provenance(provenance));
            }
            // Through that same mapping, for that same reason: a skill
            // expansion's files mean to egress exactly what a `read` of them
            // means. The empty-set/`!unknown` case — ordinary typed prompt text
            // — contributes nothing and takes neither branch.
            CtxProvenance::User { sources, unknown } => {
                if !sources.is_empty() {
                    prov.merge(&tool_result_provenance(&ToolProvenance::Sources(
                        sources.clone(),
                    )));
                }
                if *unknown {
                    prov.merge(&tool_result_provenance(&ToolProvenance::Unknown));
                }
            }
            CtxProvenance::System | CtxProvenance::Model => {}
        }
    }
    // Through the same mapping, for the same reason: the forgotten blocks are
    // scoped by exactly the rule the surviving ones are.
    let dropped = ctx.dropped_provenance();
    if !dropped.sources().is_empty() {
        prov.merge(&tool_result_provenance(&ToolProvenance::Sources(
            dropped.sources().clone(),
        )));
    }
    if dropped.is_unknown() {
        prov.merge(&tool_result_provenance(&ToolProvenance::Unknown));
    }
    prov
}

/// The [`ToolSpec`]s for the tools exposed under a `max_tools` cap (BR-6), for a
/// remote provider's tool list.
fn exposed_tool_specs(tools: &ToolRegistry, max_tools: Option<u32>) -> Vec<ToolSpec> {
    tools
        .exposed_names(max_tools)
        .iter()
        .filter_map(|name| tools.get(name))
        .map(|tool| ToolSpec {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.input_schema(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;
    use std::sync::Arc;

    use crate::harness::context::{StructuredMessage, ToolProvenance};
    use crate::harness::turn_loop::ContextRefusalOrigin;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::BTreeSet;
    use teton_inference::{Completion, GenParams, MockEngine};
    use teton_providers::{
        CapabilityProfile, ProviderError, StopReason, ToolCall, TransportError, TransportRequest,
        TransportResponse, TurnCompletion, TurnStream,
    };

    use crate::egress::NoopSink;

    /// A [`PreparedPrompt`] with just a flat body — the shape the local source
    /// consumes in these tests (the remote-shaping fields are exercised by the
    /// integration tests and `prepared_prompt_carries_*` below).
    fn flat_prompt(text: &str) -> PreparedPrompt {
        PreparedPrompt {
            flat: text.to_owned(),
            system: String::new(),
            messages: Vec::new(),
        }
    }

    /// An [`Engine`] that records the exact prompt string it is handed and
    /// reports a configured [`ChatFormat`].
    ///
    /// `MockEngine::with_chat_format` reports the format but throws the prompt
    /// away, and the prompt is precisely what REQ-554 pins: AC-5's CI check is
    /// that the string the engine tokenizes — and therefore window-checks — IS
    /// the rendered one, which is only observable by capturing it.
    struct CapturingEngine {
        format: ChatFormat,
        response: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    /// The shared engine handle [`LocalEngineSource`] takes, paired with the
    /// buffer the prompts it is handed land in.
    type CapturingLocal = (Arc<Mutex<dyn Engine>>, Arc<Mutex<Vec<String>>>);

    /// A [`CapturingEngine`] reporting `format` and answering `response`, ready
    /// to hand to [`LocalEngineSource::new`], and its capture buffer.
    fn capturing_engine(format: ChatFormat, response: &str) -> CapturingLocal {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(CapturingEngine {
            format,
            response: response.to_owned(),
            seen: Arc::clone(&seen),
        }));
        (engine, seen)
    }

    impl Engine for CapturingEngine {
        fn model_id(&self) -> &str {
            "capturing"
        }
        fn complete(
            &self,
            prompt: &str,
            _params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            self.seen
                .lock()
                .expect("capture mutex poisoned")
                .push(prompt.to_owned());
            on_token(&self.response);
            Ok(Completion::cold(self.response.clone(), 1, 1))
        }
        fn chat_format(&self) -> ChatFormat {
            self.format
        }
    }

    /// A user → assistant → tool-result conversation, prepared. Its *flat*
    /// rendering carries the `User:`/`Assistant:`/`Tool (` labels, so "the flat
    /// frame is absent" below tests a real difference rather than an artifact of
    /// an empty conversation.
    fn tool_using_prompt() -> PreparedPrompt {
        let mut ctx = ContextManager::new("SYSTEM PROMPT", 10_000);
        ctx.push_user("read a.rs");
        ctx.push_model(r#"{"tool":"read","arguments":{"path":"a.rs"}}"#);
        ctx.push_tool_result("read", Some(fixture_id("a.rs")), "file body");
        ctx.prepare(&mut crate::harness::context::NoopProvenanceHook)
    }

    /// The one prompt the capturing engine was handed.
    fn only_prompt(seen: &Arc<Mutex<Vec<String>>>) -> String {
        let seen = seen.lock().expect("capture mutex poisoned");
        assert_eq!(seen.len(), 1, "one turn must issue one completion");
        seen[0].clone()
    }

    /// A provider whose single turn streams some text and then **two** tool calls
    /// — the parallel-tool-call turn the reduced MVP harness deliberately
    /// collapses to just the first (documented in
    /// [`RemoteProviderSource::produce_turn`]).
    struct TwoToolProvider;

    #[async_trait]
    impl Provider for TwoToolProvider {
        fn id(&self) -> &str {
            "two-tool"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            _request: TurnRequest,
            _transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let events: Vec<Result<TurnEvent, ProviderError>> = vec![
                Ok(TurnEvent::TextDelta("planning two things ".to_owned())),
                Ok(TurnEvent::ToolCall(ToolCall {
                    id: "call-a".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({ "path": "first.rs" }),
                })),
                Ok(TurnEvent::ToolCall(ToolCall {
                    id: "call-b".to_owned(),
                    name: "edit".to_owned(),
                    arguments: json!({ "path": "second.rs" }),
                })),
                Ok(TurnEvent::Completed(TurnCompletion {
                    usage: TokenUsage {
                        input_tokens: 11,
                        output_tokens: 4,
                        reasoning_tokens: None,
                    },
                    stop_reason: StopReason::ToolUse,
                })),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// A `Transport` that is never actually reached (the mock provider ignores it),
    /// present only to satisfy [`Egress`]'s type parameter.
    struct NullTransport;

    #[async_trait]
    impl Transport for NullTransport {
        async fn execute(
            &self,
            _request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            Ok(TransportResponse {
                location: None,
                status: 200,
                body: Box::pin(futures::stream::empty()),
            })
        }
    }

    #[tokio::test]
    async fn remote_source_keeps_only_the_first_of_parallel_tool_calls() {
        // REQ-544 (parallel-tool-drop coverage): the reduced harness runs one tool
        // per turn, so when a provider emits TWO tool calls in a single turn the
        // FIRST assembled call wins and the later one is dropped. This pins that
        // documented behavior so a regression that keeps the *second* (or would
        // double-execute) is caught.
        let provider = TwoToolProvider;
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let mut source = RemoteProviderSource::new(
            &provider,
            &egress,
            "two-tool",
            "model-x",
            "sess-under-test",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let prompt = flat_prompt("prompt");
        let mut streamed = String::new();
        let turn = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |t| streamed.push_str(t),
            )
            .await
            .expect("remote turn");

        match turn.decision {
            TurnDecision::ToolCall { name, arguments } => {
                assert_eq!(
                    name, "read",
                    "the FIRST parallel tool call must win, not the second"
                );
                assert_eq!(arguments, json!({ "path": "first.rs" }));
            }
            other => panic!("expected the first tool call to be kept, got {other:?}"),
        }
        // The dropped second call is counted so the loop can tell the model.
        assert_eq!(turn.dropped_calls, 1);
        // The turn's text streamed through and usage came from the terminal event.
        assert!(streamed.contains("planning two things"));
        assert_eq!(
            turn.usage,
            TokenUsage {
                input_tokens: 11,
                output_tokens: 4,
                reasoning_tokens: None,
            }
        );
    }

    // ---- BUG-180: a remote provider's text-form tool call -----------------

    /// A provider that streams **only text** — `chunks`, in order, then a
    /// terminal `Completed` — and never a structured `TurnEvent::ToolCall`.
    /// This is the shape of a model that obeyed the system prompt's "reply with
    /// ONLY a JSON object" over the API's native tool field (BUG-180: Kimi K3,
    /// `edit`-routed, 2026-08-19). Each call to `stream_turn` serves the next
    /// scripted reply, so one provider can drive a whole loop turn.
    struct TextOnlyProvider {
        replies: Mutex<std::collections::VecDeque<Vec<&'static str>>>,
    }

    impl TextOnlyProvider {
        fn scripted(replies: Vec<Vec<&'static str>>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl Provider for TextOnlyProvider {
        fn id(&self) -> &str {
            "text-only"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            _request: TurnRequest,
            _transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let chunks = self
                .replies
                .lock()
                .expect("script mutex poisoned")
                .pop_front()
                .expect("the script ran out of replies");
            let mut events: Vec<Result<TurnEvent, ProviderError>> = chunks
                .into_iter()
                .map(|c| Ok(TurnEvent::TextDelta(c.to_owned())))
                .collect();
            events.push(Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage {
                    input_tokens: 9,
                    output_tokens: 3,
                    reasoning_tokens: None,
                },
                // What a provider reports for a reply it considers prose.
                stop_reason: StopReason::EndTurn,
            })));
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// A provider that refuses the reasoning-effort field on its first call and
    /// the request's **size** on its second — the two refusals REQ-559 BR-12
    /// and REQ-586 BR-2 put back to back on one turn.
    ///
    /// Both are real: an effort refusal is answered by exactly one retry with
    /// no reasoning field, and that retry carries the same oversized prompt, so
    /// a provider whose window the context busts refuses it again for the other
    /// reason. Every request's effort is recorded, so the test can tell the
    /// retry from the first attempt.
    #[derive(Default)]
    struct RefusesEffortThenSize {
        seen: Mutex<Vec<ResolvedEffort>>,
    }

    #[async_trait]
    impl Provider for RefusesEffortThenSize {
        fn id(&self) -> &str {
            "refuser"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            request: TurnRequest,
            _transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let mut seen = self.seen.lock().expect("call log mutex");
            seen.push(request.effort);
            if seen.len() == 1 {
                return Err(ProviderError::EffortRefused {
                    provider_id: "refuser".to_owned(),
                    requested: teton_core::EffortLevel::High,
                    clamped: teton_core::EffortLevel::High,
                });
            }
            Err(ProviderError::ContextLengthExceeded {
                provider_id: "refuser".to_owned(),
            })
        }
    }

    /// **Verify M2.** A context-length refusal that arrives on the REQ-559
    /// effort-retry is the *same* typed outcome it is on the first attempt.
    ///
    /// The retry used to propagate with `?`, and [`HarnessError::Remote`] is
    /// `#[from] ProviderError`, so this refusal became `Remote(..)`. That is
    /// the arm the runtime treats as "this provider failed the turn" — and
    /// `ContextLengthExceeded` deliberately has **no** failure class, so the
    /// runtime's `Remote(perr) if attempts < 2` arm found nothing to act on and
    /// the user got `INTERNAL_ERROR "provider failed unrecoverably"` instead of
    /// the window/size report. The variant is the whole assertion: the numbers
    /// were never the part that broke.
    #[tokio::test]
    async fn a_context_length_refusal_on_the_effort_retry_stays_typed() {
        let provider = RefusesEffortThenSize::default();
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let mut source = RemoteProviderSource::new(
            &provider,
            &egress,
            "refuser",
            "model-x",
            "sess-under-test",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let config = HarnessConfig::default();
        let prompt = PreparedPrompt {
            flat: String::new(),
            system: "one two three".to_owned(),
            messages: vec![StructuredMessage {
                role: MessageRole::User,
                text: "four five six seven".to_owned(),
            }],
        };

        let err = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &config,
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect_err("a refused turn must not report success");

        match &err {
            HarnessError::ContextLengthExceeded {
                provider_id,
                assembled_tokens,
                budget_tokens,
            } => {
                assert_eq!(provider_id, "refuser");
                assert_eq!(*budget_tokens, config.context_budget_tokens);
                assert_eq!(
                    *assembled_tokens, 7,
                    "the report names what this attempt actually assembled"
                );
            }
            other => panic!(
                "a context-length refusal on the retry must stay typed; as \
                 `Remote` the daemon reports it as an opaque internal error: \
                 {other:?}"
            ),
        }

        // Non-vacuity: the retry really is the call that refused. One request
        // with the effort field, one without, and nothing after it.
        let seen = provider.seen.lock().expect("call log mutex");
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(matches!(seen[0], ResolvedEffort::Effort { .. }), "{seen:?}");
        assert!(matches!(seen[1], ResolvedEffort::Omit { .. }), "{seen:?}");
        assert!(
            source.effort_was_refused(),
            "the session memo still records the effort refusal that happened"
        );
    }

    /// Drive one remote turn over `provider` and return it with what streamed.
    async fn remote_turn_over(provider: &TextOnlyProvider) -> (SourceTurn, String) {
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let mut source = RemoteProviderSource::new(
            provider,
            &egress,
            "text-only",
            "model-x",
            "sess-under-test",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let prompt = flat_prompt("prompt");
        let mut streamed = String::new();
        let turn = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |t| streamed.push_str(t),
            )
            .await
            .expect("remote turn");
        (turn, streamed)
    }

    /// **BUG-180.** The model wrote its call in the text grammar the system
    /// prompt teaches and sent no native call. Before the fix this was
    /// `EndTurn` with the JSON as the answer — which the display gate then hid,
    /// so the user saw an empty turn and no tool ran. It is a tool call,
    /// recognized by the same grammar the local tier uses, and the text ends
    /// with it (`call_in_text`) so the loop does not render it on twice.
    #[tokio::test]
    async fn a_remote_text_form_tool_call_is_a_call_not_an_answer() {
        let provider = TextOnlyProvider::scripted(vec![vec![
            "{\"tool\": \"shell\", ",
            "\"arguments\": {\"command\": \"ls -la ~/.claude/skills\"}}",
            // A continuation past the call — a fabricated result, say — is
            // cut from the text exactly as a local reply's would be.
            "\nTool (shell):\nfake listing\n",
        ]]);
        let (turn, _streamed) = remote_turn_over(&provider).await;

        match &turn.decision {
            TurnDecision::ToolCall { name, arguments } => {
                assert_eq!(name, "shell");
                assert_eq!(arguments, &json!({ "command": "ls -la ~/.claude/skills" }));
            }
            other => panic!("a text-form call must be dispatched, got {other:?}"),
        }
        assert!(
            turn.call_in_text,
            "the call is in the text, so the loop must not append it again"
        );
        assert!(
            turn.text.ends_with("\"ls -la ~/.claude/skills\"}}"),
            "the text is cut at the call's end, got {:?}",
            turn.text
        );
        assert!(
            !turn.text.contains("fake listing"),
            "the continuation past the call never reaches context"
        );
        assert_eq!(turn.dropped_calls, 0);
        // The usage still comes from the terminal event, unchanged.
        assert_eq!(turn.usage.output_tokens, 3);
    }

    /// **BUG-180.** A text-form call to a tool that does not exist is a
    /// correction for the model, not the turn's answer — the same `Malformed`
    /// path a local reply takes, under the same turn ceiling.
    #[tokio::test]
    async fn a_remote_text_form_call_to_an_unknown_tool_is_malformed() {
        let provider =
            TextOnlyProvider::scripted(vec![vec!["{\"tool\":\"teleport\",\"arguments\":{}}"]]);
        let (turn, _) = remote_turn_over(&provider).await;
        match &turn.decision {
            TurnDecision::Malformed { reason } => {
                assert!(reason.contains("teleport"), "{reason}");
            }
            other => panic!("an unknown tool must be folded back, got {other:?}"),
        }
        assert!(
            !turn.call_in_text,
            "no call was accepted, so none is embedded"
        );
    }

    /// **BUG-180, the other direction.** Prose is still prose: an object the
    /// model merely *quotes*, with no `tool`/`name` key, does not become a
    /// call, and the turn ends with the whole text as its answer.
    #[tokio::test]
    async fn remote_prose_quoting_a_non_call_object_still_ends_the_turn() {
        let provider = TextOnlyProvider::scripted(vec![vec![
            "The config is:\n",
            "{\"port\": 8080}",
            "\nas requested.",
        ]]);
        let (turn, streamed) = remote_turn_over(&provider).await;
        assert_eq!(
            turn.decision,
            TurnDecision::EndTurn {
                final_text: "The config is:\n{\"port\": 8080}\nas requested.".to_owned()
            }
        );
        assert_eq!(turn.text, "The config is:\n{\"port\": 8080}\nas requested.");
        assert_eq!(streamed, turn.text, "prose streams through untouched");
        assert!(!turn.call_in_text);
    }

    /// **BUG-180, precedence.** A native call still wins outright, and the
    /// prose beside it is left alone however call-shaped some JSON in it may
    /// look (REQ-567 OQ-1): the loop renders the native call onto the prose,
    /// so the source must not also read a call out of it.
    #[tokio::test]
    async fn a_native_call_wins_and_leaves_call_shaped_prose_alone() {
        struct NativeWithQuotedJson;

        #[async_trait]
        impl Provider for NativeWithQuotedJson {
            fn id(&self) -> &str {
                "native"
            }
            fn capabilities(&self) -> CapabilityProfile {
                CapabilityProfile::default()
            }
            async fn stream_turn(
                &self,
                _request: TurnRequest,
                _transport: &dyn Transport,
            ) -> Result<TurnStream, ProviderError> {
                let events: Vec<Result<TurnEvent, ProviderError>> = vec![
                    Ok(TurnEvent::TextDelta(
                        "Cargo says {\"name\": \"serde\", \"version\": \"1\"} here. ".to_owned(),
                    )),
                    Ok(TurnEvent::ToolCall(ToolCall {
                        id: "call-a".to_owned(),
                        name: "read".to_owned(),
                        arguments: json!({ "path": "Cargo.toml" }),
                    })),
                    Ok(TurnEvent::Completed(TurnCompletion {
                        usage: TokenUsage::default(),
                        stop_reason: StopReason::ToolUse,
                    })),
                ];
                Ok(Box::pin(futures::stream::iter(events)))
            }
        }

        let provider = NativeWithQuotedJson;
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let mut source = RemoteProviderSource::new(
            &provider,
            &egress,
            "native",
            "model-x",
            "sess-under-test",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let turn = source
            .produce_turn(
                &flat_prompt("prompt"),
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("remote turn");
        assert_eq!(
            turn.decision,
            TurnDecision::ToolCall {
                name: "read".to_owned(),
                arguments: json!({ "path": "Cargo.toml" }),
            }
        );
        assert!(!turn.call_in_text, "a native call is not in the text");
        assert_eq!(
            turn.text, "Cargo says {\"name\": \"serde\", \"version\": \"1\"} here. ",
            "the prose beside a native call is untouched"
        );
    }

    /// **BUG-180, through the loop.** What the user saw on 2026-08-19: a turn
    /// that ended with nothing on screen and no tool run. Driven here by a real
    /// [`RemoteProviderSource`] over a provider that writes its call as text:
    /// the tool status line is presented, the tool runs, the model is called
    /// again with the result, and the raw JSON never reaches the user.
    #[tokio::test]
    async fn a_remote_text_form_call_runs_the_tool_instead_of_ending_the_turn_silently() {
        use crate::broadcast::EventBus;
        use crate::harness::context::{BlockRole, NoopProvenanceHook};
        use crate::harness::duty::DutyRoute;
        use crate::harness::permissions::{PendingPermissions, PermissionConfig, PermissionGate};
        use crate::harness::tools::{ToolContext, ToolDuties};
        use crate::harness::turn_loop::{run_session_turn_with_source, SessionEvents};
        use teton_protocol::events::{Event, SessionUpdate, SessionUpdatePayload};
        use teton_protocol::methods::StopReason as TurnStop;

        let provider = TextOnlyProvider::scripted(vec![
            vec!["{\"tool\":\"read\",\"arguments\":{\"path\":\"nope.txt\"}}"],
            vec!["Done."],
        ]);
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let mut source = RemoteProviderSource::new(
            &provider,
            &egress,
            "text-only",
            "model-x",
            "bug180",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );

        let session_id = SessionId::from("bug180");
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(256);
        let gate = PermissionGate::new(
            session_id.clone(),
            PermissionConfig::permissive(),
            Arc::clone(&bus),
            Arc::new(PendingPermissions::new()),
        );
        let events = SessionEvents::new(Arc::clone(&bus), session_id);
        let config = HarnessConfig::default();
        let tools = ToolRegistry::with_builtins();
        let tool_ctx = ToolContext::new(std::env::temp_dir());
        let mut hook = NoopProvenanceHook;
        let mut ctx = ContextManager::new("sys", config.context_budget_tokens)
            .with_budget_bytes(config.context_budget_bytes);
        ctx.push_user("show me the skills");

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
        .await
        .expect("the turn completes");

        assert_eq!(outcome.stop_reason, TurnStop::EndTurn);
        assert_eq!(
            outcome.turns, 2,
            "the tool ran and the model was called again"
        );
        assert_eq!(outcome.final_text, "Done.");

        // The tool status line presented the call, and the JSON never streamed.
        let mut titles = Vec::new();
        let mut shown = String::new();
        while let Some(env) = sub.try_recv() {
            match &env.event {
                Event::SessionUpdate(SessionUpdate {
                    update: SessionUpdatePayload::ToolCall { title, .. },
                }) => titles.push(title.clone()),
                Event::SessionUpdate(SessionUpdate {
                    update: SessionUpdatePayload::AgentMessageChunk { text },
                }) => shown.push_str(text),
                _ => {}
            }
        }
        assert_eq!(titles, vec!["read nope.txt".to_owned()]);
        assert!(
            !shown.contains("{\"tool\""),
            "raw tool-call JSON must not reach the user, got {shown:?}"
        );
        assert!(shown.contains("Done."));

        // The assistant block for the call turn ends with the call, once —
        // `call_in_text` kept the loop from rendering it on a second time.
        let assistant: Vec<&str> = ctx
            .blocks()
            .iter()
            .filter(|b| b.role == BlockRole::Assistant)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(assistant.len(), 2, "{assistant:?}");
        assert_eq!(
            assistant[0],
            "{\"tool\":\"read\",\"arguments\":{\"path\":\"nope.txt\"}}"
        );
        assert_eq!(assistant[0].matches("\"tool\"").count(), 1);
    }

    /// The native tool-spec projection sees the same exposure the prompt does —
    /// the cap bounding the non-exempt tools, and the cap-exempt `teton_docs`
    /// riding through it (REQ-577 BR-7). A projection that applied the cap its
    /// own way would give a native tool-caller a different tool set than the
    /// prompt describes.
    #[test]
    fn exposed_tool_specs_respects_the_cap() {
        let tools = ToolRegistry::with_builtins();
        let specs = exposed_tool_specs(&tools, Some(2));
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["read", "edit", "teton_docs"]);
        // Each spec carries a description and a schema the provider can serialize.
        assert!(!specs[0].description.is_empty());
        assert!(specs[0].input_schema.is_object());
    }

    /// **Seam 2 of three (REQ-585 BR-7, ADR-9).** The union is over every block
    /// that carries file provenance — which now includes a **user** block, because
    /// a `/skill` expansion is prompt text by role and file content by origin and
    /// has to pin the turn exactly as a `read` of that file would.
    ///
    /// This test was named `context_provenance_unions_tool_result_paths_only`,
    /// and that name *was* the claim BR-7 breaks: it is renamed and re-asserted
    /// rather than deleted, so the seam keeps a witness across the change.
    /// Typed prompt text still contributes nothing, and the empty set is what
    /// says so — which is precisely why `unknown` could not be encoded inside it.
    #[test]
    fn context_provenance_unions_tool_result_and_skill_expansion_sources() {
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_user("do the thing");
        ctx.push_model("{\"tool\":\"read\"}");
        ctx.push_tool_result("read", Some(fixture_id("src/lib.rs")), "code");
        ctx.push_tool_result("read", Some(fixture_id("secrets/prod.env")), "API_KEY=1");
        // A tool result with no touched files (e.g. a benign status) contributes
        // nothing and is not unknown.
        ctx.push_tool_result("shell", None, "ok");
        // A skill expansion: a user block whose text came out of a repo file.
        ctx.push_user_from(
            "run the release checklist",
            [fixture_id(".claude/skills/release/SKILL.md")]
                .into_iter()
                .collect(),
            false,
        );

        let prov = context_provenance(&ctx);
        assert_eq!(prov.len(), 3, "{prov:?}");
        assert!(prov.contains("src/lib.rs"));
        assert!(prov.contains("secrets/prod.env"));
        assert!(
            prov.contains(".claude/skills/release/SKILL.md"),
            "a skill expansion must pin the turn as a read of its file would"
        );
        assert!(!prov.is_unknown());
    }

    /// **ADR-9's id-minting gap.** A skill file with no mintable identity — a
    /// user-scoped `~/.claude/skills/x/SKILL.md` in a repo-rooted session, which
    /// `ProvenanceId::from_resolved` refuses by design (REQ-571 ADR-B) — sets
    /// `unknown` on its user block, and the whole context fail-closes, exactly
    /// as a `shell` result does. The alternative would be a file outside the
    /// root silently counting as unpinnable.
    ///
    /// The empty set beside the bit is the point: this block names no file *and*
    /// is not ordinary typed prose, and only two fields can say both.
    #[test]
    fn a_user_block_whose_sources_cannot_be_minted_makes_the_context_unknown() {
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_tool_result("read", Some(fixture_id("src/lib.rs")), "code");
        ctx.push_user_from("run my personal skill", BTreeSet::new(), true);

        let prov = context_provenance(&ctx);
        assert!(
            prov.is_unknown(),
            "an unpinnable skill expansion must fail the turn closed"
        );
        assert!(!prov.is_empty(), "unknown provenance is never empty");
        // Known sources are still carried alongside the unknown bit — the pair,
        // not one collapsed into the other.
        assert!(prov.contains("src/lib.rs"));
    }

    #[test]
    fn context_provenance_is_unknown_when_any_result_is_unknown() {
        // REQ-544 C-1: a `shell` result folds in as UNKNOWN, which makes the whole
        // context's provenance unknown → egress fail-closes on it.
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_tool_result("read", Some(fixture_id("src/lib.rs")), "code");
        ctx.push_tool_result_prov(
            "shell",
            ToolProvenance::Unknown,
            "cat secrets/prod.env output",
        );

        let prov = context_provenance(&ctx);
        assert!(
            prov.is_unknown(),
            "an unknown result must taint the context"
        );
        assert!(!prov.is_empty(), "unknown provenance is never empty");
        // Known sources are still carried alongside the unknown bit.
        assert!(prov.contains("src/lib.rs"));
    }

    /// **Seam 4 of four (REQ-612 BR-5, ADR-2, AC-7's unit half).** The union is
    /// also over the identities the *system prompt* carries, which before this
    /// REQ was a set that could not exist: `CtxProvenance::System` contributes
    /// nothing and a repository file placed in the prompt would therefore have
    /// egressed with no boundary verdict on any turn.
    ///
    /// Three claims, and the empty-set one is what says the addition is inert
    /// for every session with no notes — which is every session in the tree's
    /// other 2,000 tests.
    ///
    /// **Mutation, run red.** Deleting the `system_sources` union in
    /// `context_provenance` fails the first two assertions with `len` 1 against
    /// 2 and a missing `TETON.md`. It is the mutation AC-7 names ("a test that
    /// makes `context_provenance` ignore the block fails"), and it is checked
    /// here rather than asserted about.
    #[test]
    fn context_provenance_unions_the_system_sources() {
        // A manager whose *conversation* is nothing but typed prose: with no
        // union the answer would be empty, so nothing here can pass by
        // borrowing another block's identity.
        let mut ctx =
            ContextManager::new("system", 10_000).with_system_sources([fixture_id("TETON.md")]);
        ctx.push_user("what does this repo build with?");
        ctx.push_model("cargo.");
        ctx.push_tool_result("read", Some(fixture_id("src/lib.rs")), "code");

        let prov = context_provenance(&ctx);
        assert!(
            prov.contains("TETON.md"),
            "the notes in the prompt must pin the turn as a read of them \
             would: {prov:?}"
        );
        assert_eq!(
            prov.len(),
            2,
            "the system source unions with the blocks' rather than replacing \
             them: {prov:?}"
        );
        assert!(prov.contains("src/lib.rs"));
        assert!(
            !prov.is_unknown(),
            "a minted identity is known provenance, never a fail-closed one"
        );

        // The empty set contributes nothing: `is_unknown` stays false and the
        // length is the blocks' alone. This is what makes the addition inert
        // for a session with no `TETON.md` — the overwhelming majority.
        let mut bare = ContextManager::new("system", 10_000);
        bare.push_tool_result("read", Some(fixture_id("src/lib.rs")), "code");
        let bare_prov = context_provenance(&bare);
        assert_eq!(bare_prov.len(), 1, "{bare_prov:?}");
        assert!(!bare_prov.is_unknown());
        assert!(bare_prov.contains("src/lib.rs"));
    }

    /// The half of ADR-9 that a one-field `Provenance::User` would have broken:
    /// an ordinary typed prompt is the empty set with `unknown` clear, and it
    /// pins nothing. Encoding "unpinnable" as the empty set would make this
    /// context unknown and pin every typed prompt on every machine with a
    /// boundary configured.
    #[test]
    fn a_context_of_only_prompt_text_has_empty_provenance() {
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_user("just a question");
        ctx.push_model("just an answer");
        let prov = context_provenance(&ctx);
        assert!(prov.is_empty());
        assert!(!prov.is_unknown());
    }

    #[tokio::test]
    async fn local_source_parses_a_tool_call_and_streams_the_text() {
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::with_response(
            "mock",
            r#"{"tool":"read","arguments":{"path":"a.rs"}}"#,
        )));
        let mut source =
            LocalEngineSource::new(engine, ChatFormat::Flat, SessionId::from("test-session"));
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let mut streamed = String::new();
        let prompt = flat_prompt("prompt");
        let turn = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |t| streamed.push_str(t),
            )
            .await
            .expect("local turn");
        assert!(streamed.contains("read"), "the turn text was streamed out");
        match turn.decision {
            TurnDecision::ToolCall { name, .. } => assert_eq!(name, "read"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_source_reports_plain_text_as_end_of_turn() {
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::with_response(
            "mock",
            "All done, nothing more to do.",
        )));
        let mut source =
            LocalEngineSource::new(engine, ChatFormat::Flat, SessionId::from("test-session"));
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let prompt = flat_prompt("prompt");
        let turn = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("local turn");
        match turn.decision {
            TurnDecision::EndTurn { final_text } => assert!(final_text.contains("All done")),
            other => panic!("expected end of turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_chatml_engine_is_handed_the_rendered_prompt_not_the_flat_one() {
        // REQ-554 AC-1 / AC-5's CI pin: the string handed to `Engine::complete`
        // — the one the engine tokenizes and window-checks — is the ChatML
        // *rendering*, not `prompt.flat`. Capturing it is the only way to see
        // that; a test that only asserted the turn parsed would pass with the
        // flat string still going out.
        let (engine, seen) = capturing_engine(
            ChatFormat::ChatMl,
            r#"{"tool":"read","arguments":{"path":"a.rs"}}"#,
        );
        let mut source =
            LocalEngineSource::new(engine, ChatFormat::ChatMl, SessionId::from("test-session"));
        assert_eq!(
            source.chat_format(),
            ChatFormat::ChatMl,
            "the source must adopt the committed engine's format (ADR-2)"
        );
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let prompt = tool_using_prompt();

        let turn = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("local turn");

        let sent = only_prompt(&seen);
        assert!(
            sent.contains("<|im_start|>"),
            "the engine was not shown the template's role delimiters: {sent}"
        );
        assert_eq!(
            sent,
            render::render_prompt(ChatFormat::ChatMl, &prompt),
            "the engine must receive exactly the renderer's output"
        );
        // The flat frame must not survive into template mode — and the flat
        // rendering of this same prompt does carry it, so this is a real
        // difference (BUG-147: a model shown `Tool (` writes `Tool (`).
        assert!(!sent.contains("\nUser:\n"), "flat user label leaked");
        assert!(
            !sent.contains("\nAssistant:\n"),
            "flat assistant label leaked"
        );
        assert!(!sent.contains("\nTool ("), "flat tool label leaked");
        assert!(prompt.flat.contains("\nUser:\n") && prompt.flat.contains("\nTool (read):\n"));
        // ...and the turn still parses: the containment runs in template mode.
        match turn.decision {
            TurnDecision::ToolCall { name, .. } => assert_eq!(name, "read"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_flat_engine_is_handed_prompt_flat_byte_for_byte() {
        // BR-2: the fallback is not a re-derivation — the engine gets the exact
        // string `prepare()` already produced, which is what keeps every
        // scripted fixture and the flat `{{LAST_TOOL_RESULT}}` parsing valid.
        let (engine, seen) = capturing_engine(ChatFormat::Flat, "All done.");
        let mut source =
            LocalEngineSource::new(engine, ChatFormat::Flat, SessionId::from("test-session"));
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let prompt = tool_using_prompt();

        source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("local turn");

        assert_eq!(only_prompt(&seen), prompt.flat);
        // AC-7: a plain test double inherits `Flat` from the trait default, so
        // it takes this same path with no edit of its own.
        let mock: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(MockEngine::new("mock")));
        assert_eq!(
            LocalEngineSource::new(mock, ChatFormat::Flat, SessionId::from("test-session"))
                .chat_format(),
            ChatFormat::Flat
        );
    }

    /// An engine that mirrors `LlamaEngine`'s over-window refusal in byte
    /// currency: `complete` returns [`EngineError::ContextWindowExceeded`] —
    /// the same variant `teton_inference::over_window` produces in production —
    /// when the prompt it is handed exceeds `window_bytes`. Reports ChatML so
    /// the source renders.
    ///
    /// The **variant** is what production shares; the currency is not. A real
    /// engine measures tokenized prompts, and this double has no tokenizer, so
    /// it fills the token-named fields with byte counts. That is honest here
    /// because the harness discards those numbers (REQ-589 ADR-3) — what it
    /// reports is its own word estimate against its own budget — so nothing
    /// downstream can be misled by the substitution.
    struct WindowedEngine {
        window_bytes: usize,
        format: ChatFormat,
    }

    impl Engine for WindowedEngine {
        fn model_id(&self) -> &str {
            "windowed"
        }
        fn chat_format(&self) -> ChatFormat {
            self.format
        }
        fn complete(
            &self,
            prompt: &str,
            _params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<teton_inference::Completion, EngineError> {
            if prompt.len() > self.window_bytes {
                let measured = u32::try_from(prompt.len()).unwrap_or(u32::MAX);
                let window = u32::try_from(self.window_bytes).unwrap_or(u32::MAX);
                return Err(EngineError::ContextWindowExceeded {
                    prompt_tokens: measured,
                    budget_tokens: window,
                    n_ctx: window,
                    max_tokens: 0,
                });
            }
            on_token("ok");
            Ok(teton_inference::Completion::cold("ok".to_owned(), 1, 1))
        }
    }

    /// An engine whose inference simply falls over — the *other* way a local
    /// turn fails, and the non-vacuity partner of the window-refusal double.
    struct FailingEngine;

    impl Engine for FailingEngine {
        fn model_id(&self) -> &str {
            "failing"
        }
        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<teton_inference::Completion, EngineError> {
            Err(EngineError::Backend("the tier fell over".to_owned()))
        }
    }

    #[tokio::test]
    async fn an_over_window_rendered_prompt_is_refused_with_the_typed_error() {
        // AC-5, pinned in the always-on suite (REQ-554 verify): a prompt sized
        // to fit the window as flat text but to CROSS it once ChatML overhead
        // is added must be refused with the typed error — proving the
        // window-checked string is the RENDERED one, template overhead
        // included, and that the refusal is an error, never a crash.
        //
        // REQ-589 ADR-3 / AC-1: and that typed error is now the **context
        // outcome**, not a generic engine failure. Until this REQ the local
        // tier's window refusal left here as `Engine(Backend(..))`, which the
        // daemon reports as `INTERNAL_ERROR` — so BR-3, BR-12 and BR-14.1 had
        // no backstop on the one tier the reported failure ran on.
        let content = "a".repeat(100);
        let prompt = PreparedPrompt {
            flat: content.clone(),
            system: String::new(),
            messages: vec![crate::harness::context::StructuredMessage {
                role: MessageRole::User,
                text: content,
            }],
        };
        // Flat length 100; ChatML adds the user wrap + generation cue
        // (12+4+1+11+22 = 50 bytes). A 120-byte window admits one, not the other.
        let window_bytes = 120;

        let chatml: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(WindowedEngine {
            window_bytes,
            format: ChatFormat::ChatMl,
        }));
        let mut source =
            LocalEngineSource::new(chatml, ChatFormat::ChatMl, SessionId::from("test-session"));
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let config = HarnessConfig::default();
        let err = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &config,
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect_err("the rendered prompt crosses the window");
        match &err {
            HarnessError::LocalContextLengthExceeded {
                assembled_tokens,
                budget_tokens,
            } => {
                // The harness's own currency on both sides of the gap: one
                // whitespace word of content, against the budget this attempt
                // actually ran under. The engine's byte figures are not here,
                // and must not be — see `classify_engine_failure`.
                assert_eq!(*assembled_tokens, 1, "{err:?}");
                assert_eq!(*budget_tokens, config.context_budget_tokens, "{err:?}");
            }
            other => panic!(
                "a local window refusal must be the typed context outcome; as \
                 `Engine` the daemon reports it as an opaque internal error and \
                 names no remedy: {other:?}"
            ),
        }
        // And it reaches a consumer through the one tier-agnostic projection,
        // naming the local origin rather than a provider it does not have.
        assert_eq!(
            err.context_refusal().map(|refusal| refusal.origin),
            Some(ContextRefusalOrigin::LocalEngine),
            "{err:?}"
        );

        // The SAME prompt under flat rendering fits — the overhead is what
        // crossed the window.
        let flat: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(WindowedEngine {
            window_bytes,
            format: ChatFormat::Flat,
        }));
        let mut source =
            LocalEngineSource::new(flat, ChatFormat::Flat, SessionId::from("test-session"));
        source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &config,
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("the flat rendering fits the same window");
    }

    /// **REQ-589 AC-4's non-vacuity.** The classification is by variant, so the
    /// *other* way a local turn fails is untouched: an inference failure is
    /// still [`HarnessError::Engine`], and is still not a window refusal.
    ///
    /// The pair matters more than either half. A blanket conversion — every
    /// engine error becoming the context outcome — would pass the test above
    /// and would tell a user whose backend died to go and raise a window.
    #[tokio::test]
    async fn a_local_inference_failure_is_not_a_window_refusal() {
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(FailingEngine));
        let mut source =
            LocalEngineSource::new(engine, ChatFormat::Flat, SessionId::from("test-session"));
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let err = source
            .produce_turn(
                &tool_using_prompt(),
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect_err("the engine fell over");
        assert!(
            matches!(&err, HarnessError::Engine(EngineError::Backend(m)) if m == "the tier fell over"),
            "an inference failure must keep the shape it has always had, got {err:?}"
        );
        assert!(
            err.context_refusal().is_none(),
            "an inference failure is not a window refusal: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_remote_source_reports_the_flat_default() {
        // ADR-4: fabrication markers describe a *local text* rendering. A remote
        // turn's tool calls arrive as structured events, so the remote source
        // stays on the trait default and the turn loop's gate is unchanged for
        // it.
        let provider = TwoToolProvider;
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let source = RemoteProviderSource::new(
            &provider,
            &egress,
            "two-tool",
            "model-x",
            "sess-under-test",
            ResolvedEffort::effort(teton_core::EffortLevel::High),
        );
        assert_eq!(source.chat_format(), ChatFormat::Flat);
    }

    #[test]
    fn prepared_prompt_carries_system_and_alternating_role_typed_messages() {
        // REQ-544 M-8: the structured rendering a remote turn maps to the wire is
        // a non-empty system prompt plus alternating user/assistant messages —
        // NOT one collapsed user blob. Tool results ride as user turns; a model
        // turn is preserved as an assistant turn.
        let mut ctx = ContextManager::new("SYSTEM PROMPT", 10_000);
        ctx.push_user("please edit the file");
        ctx.push_model(r#"{"tool":"read","arguments":{"path":"a.rs"}}"#);
        ctx.push_tool_result("read", Some(fixture_id("a.rs")), "file body");

        let mut hook = crate::harness::context::NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook);

        // A real, non-empty system field (not None).
        assert_eq!(prepared.system, "SYSTEM PROMPT");

        // Multiple role-typed messages, strictly alternating and starting with a
        // user turn — proof the blob was not collapsed.
        assert_eq!(prepared.messages.len(), 3);
        assert_eq!(prepared.messages[0].role, MessageRole::User);
        assert!(prepared.messages[0].text.contains("please edit the file"));
        assert_eq!(prepared.messages[1].role, MessageRole::Assistant);
        assert!(prepared.messages[1].text.contains("\"tool\":\"read\""));
        // The tool result is folded into a user turn, annotated so the model knows
        // it is tool output.
        assert_eq!(prepared.messages[2].role, MessageRole::User);
        assert!(prepared.messages[2].text.contains("Tool result (read):"));
        assert!(prepared.messages[2].text.contains("file body"));

        // Alternation holds: no two adjacent messages share a role.
        for pair in prepared.messages.windows(2) {
            assert_ne!(pair[0].role, pair[1].role, "roles must alternate");
        }
    }

    #[test]
    fn prepared_prompt_merges_consecutive_same_role_blocks() {
        // Two user turns in a row collapse into one message so alternation holds
        // for a provider (Anthropic rejects two consecutive user messages).
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_user("first");
        ctx.push_user("second");
        let mut hook = crate::harness::context::NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook);
        assert_eq!(prepared.messages.len(), 1);
        assert_eq!(prepared.messages[0].role, MessageRole::User);
        assert!(prepared.messages[0].text.contains("first"));
        assert!(prepared.messages[0].text.contains("second"));
    }
}
