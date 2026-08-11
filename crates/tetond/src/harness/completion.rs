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

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use teton_inference::{ChatFormat, Completion, Engine, EngineError, MissReason};
use teton_protocol::events::{PrefixCache, PrefixCacheMiss, PrefixCacheOutcome};
use teton_protocol::{Category, Phase, ProviderId, SessionId};
use teton_providers::{
    Message, Provider, Role, TokenUsage, ToolSpec, Transport, TurnEvent, TurnRequest,
};

use crate::cost::{CostAttribution, LocalUsageMeter};
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::{
    ContextManager, MessageRole, PreparedPrompt, Provenance as CtxProvenance, ToolProvenance,
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
    /// Whether a [`TurnDecision::ToolCall`] on this turn is **embedded in
    /// [`text`](Self::text)** rather than arriving beside it (REQ-567 OQ-1).
    ///
    /// The two sources answer differently and the difference is structural, not
    /// stylistic:
    ///
    /// - [`LocalEngineSource`] parses the call **out of the reply text**, so the
    ///   assistant block the loop pushes literally contains the call's JSON.
    /// - [`RemoteProviderSource`] receives the call as a structured
    ///   [`TurnEvent::ToolCall`]; the text is prose only, and any JSON in it is
    ///   something the model was *talking about*.
    ///
    /// The loop needs the answer because OQ-1's cancellation trim edits that
    /// block's text. Re-deriving it later by re-parsing the text is what this
    /// field exists to prevent: a cancelled remote turn whose prose mentions
    /// `{"name": "serde", "version": "1"}` is tool-call-*shaped* and would be
    /// truncated at that JSON, discarding content the user watched stream.
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
    /// fabrication markers are a *local text parsing* concern — a remote turn's
    /// tool calls arrive as structured [`TurnEvent::ToolCall`] events and its
    /// prose as `TextDelta`s, so there is no transcript rendering in the text for
    /// a marker to describe, and the provider owns its own wire format. The
    /// default also keeps every test source compiling unchanged.
    fn chat_format(&self) -> ChatFormat {
        ChatFormat::Flat
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
        let (model, completion) = task.await.map_err(|_| {
            EngineError::Backend("the local inference task did not complete".to_owned())
        })??;
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
    ) -> Self {
        Self {
            provider,
            egress,
            provider_id: provider_id.into(),
            model: model.into(),
            session_id: session_id.into(),
            phase: None,
            category: None,
        }
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
}

#[async_trait]
impl<T: Transport> CompletionSource for RemoteProviderSource<'_, T> {
    async fn produce_turn(
        &mut self,
        prompt: &PreparedPrompt,
        provenance: &Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        _exposed: &[&str],
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
        let egress_ctx = EgressContext::new(self.provider_id.clone())
            .with_session(self.session_id.clone())
            .with_cost(attribution);
        let transport = self.egress.scoped(provenance.clone(), egress_ctx);

        // Errors known at open time (including a privacy block, surfaced as a
        // transport refusal) come back here before any events flow.
        let mut stream = self.provider.stream_turn(request, &transport).await?;

        let mut text = String::new();
        let mut tool_call: Option<TurnDecision> = None;
        let mut dropped_calls = 0u32;
        let mut usage = TokenUsage::default();
        while let Some(event) = stream.next().await {
            match event? {
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

        let decision = tool_call.unwrap_or_else(|| TurnDecision::EndTurn {
            final_text: text.trim().to_owned(),
        });
        Ok(SourceTurn {
            text,
            decision,
            usage,
            dropped_calls,
            // A remote provider has no local KV to reuse; `None` says "this
            // source has no prefix cache", which is a different fact from a
            // miss and must not be reported as one.
            cache: None,
            // REQ-567 OQ-1: never. A remote call arrives as a structured
            // `TurnEvent::ToolCall` and is assembled above from *that*, not from
            // the stream's `TextDelta`s — so `text` is prose the user watched
            // stream and holds no call for a cancellation to trim, however
            // tool-call-shaped some JSON in it may look.
            call_in_text: false,
        })
    }
}

/// The egress [`Provenance`] of the context currently assembled in `ctx`: the
/// union of every tool result's [`ToolProvenance`].
///
/// This is the loop → egress bridge for BR-1 (REQ-544 C-1). A tool result tagged
/// with the files it touched contributes those paths; a result with UNKNOWN
/// provenance (a `shell` command) makes the whole context's provenance unknown,
/// which egress fail-closes; system/user/model blocks carry no file provenance.
/// The remote source hands the result to [`Egress::scoped`], so a turn whose
/// context touched a `local-only` file — or ran an unparseable shell command — is
/// blocked before a byte leaves.
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
#[must_use]
pub fn context_provenance(ctx: &ContextManager) -> Provenance {
    let mut prov = Provenance::empty();
    for block in ctx.blocks() {
        if let CtxProvenance::Tool { provenance, .. } = &block.provenance {
            // One per-result mapping, shared with the `digest` duty (which scopes
            // a *single* result rather than the whole context). Two spellings of
            // "what does this tool result mean to egress" is how one of them ends
            // up laxer than the other.
            prov.merge(&tool_result_provenance(provenance));
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
    use std::sync::Arc;

    use crate::harness::context::ToolProvenance;
    use async_trait::async_trait;
    use serde_json::json;
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
        ctx.push_tool_result("read", Some("a.rs".to_owned()), "file body");
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
        let mut source =
            RemoteProviderSource::new(&provider, &egress, "two-tool", "model-x", "sess-under-test");
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
                output_tokens: 4
            }
        );
    }

    #[test]
    fn exposed_tool_specs_respects_the_cap() {
        let tools = ToolRegistry::with_builtins();
        let specs = exposed_tool_specs(&tools, Some(2));
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "read");
        assert_eq!(specs[1].name, "edit");
        // Each spec carries a description and a schema the provider can serialize.
        assert!(!specs[0].description.is_empty());
        assert!(specs[0].input_schema.is_object());
    }

    #[test]
    fn context_provenance_unions_tool_result_paths_only() {
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_user("do the thing");
        ctx.push_model("{\"tool\":\"read\"}");
        ctx.push_tool_result("read", Some("src/lib.rs".to_owned()), "code");
        ctx.push_tool_result("read", Some("secrets/prod.env".to_owned()), "API_KEY=1");
        // A tool result with no touched files (e.g. a benign status) contributes
        // nothing and is not unknown.
        ctx.push_tool_result("shell", None, "ok");

        let prov = context_provenance(&ctx);
        assert_eq!(prov.len(), 2);
        assert!(prov.contains("src/lib.rs"));
        assert!(prov.contains("secrets/prod.env"));
        assert!(!prov.is_unknown());
    }

    #[test]
    fn context_provenance_is_unknown_when_any_result_is_unknown() {
        // REQ-544 C-1: a `shell` result folds in as UNKNOWN, which makes the whole
        // context's provenance unknown → egress fail-closes on it.
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_tool_result("read", Some("src/lib.rs".to_owned()), "code");
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

    #[test]
    fn a_context_of_only_prompt_text_has_empty_provenance() {
        let mut ctx = ContextManager::new("system", 10_000);
        ctx.push_user("just a question");
        ctx.push_model("just an answer");
        assert!(context_provenance(&ctx).is_empty());
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
    /// currency: `complete` returns the typed backend error when the prompt it
    /// is handed exceeds `window_bytes`. Reports ChatML so the source renders.
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
                return Err(EngineError::Backend(format!(
                    "prompt of {} bytes exceeds this engine's window ({})",
                    prompt.len(),
                    self.window_bytes
                )));
            }
            on_token("ok");
            Ok(teton_inference::Completion::cold("ok".to_owned(), 1, 1))
        }
    }

    #[tokio::test]
    async fn an_over_window_rendered_prompt_is_refused_with_the_typed_error() {
        // AC-5, pinned in the always-on suite (REQ-554 verify): a prompt sized
        // to fit the window as flat text but to CROSS it once ChatML overhead
        // is added must be refused with the typed error — proving the
        // window-checked string is the RENDERED one, template overhead
        // included, and that the refusal is an error, never a crash.
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
        let err = source
            .produce_turn(
                &prompt,
                &Provenance::empty(),
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect_err("the rendered prompt crosses the window");
        assert!(
            matches!(&err, HarnessError::Engine(EngineError::Backend(m)) if m.contains("exceeds")),
            "expected the typed over-window refusal, got {err:?}"
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
                &HarnessConfig::default(),
                &tools,
                &exposed,
                &mut |_| {},
            )
            .await
            .expect("the flat rendering fits the same window");
    }

    #[tokio::test]
    async fn a_remote_source_reports_the_flat_default() {
        // ADR-4: fabrication markers describe a *local text* rendering. A remote
        // turn's tool calls arrive as structured events, so the remote source
        // stays on the trait default and the turn loop's gate is unchanged for
        // it.
        let provider = TwoToolProvider;
        let egress = Egress::new(NullTransport, Vec::new(), Arc::new(NoopSink));
        let source =
            RemoteProviderSource::new(&provider, &egress, "two-tool", "model-x", "sess-under-test");
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
        ctx.push_tool_result("read", Some("a.rs".to_owned()), "file body");

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
