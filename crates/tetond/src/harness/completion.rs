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
//!   construction*.
//! - [`RemoteProviderSource`] — drives a [`Provider`] through the single egress
//!   choke point ([`Egress`]). The provider only ever holds the provenance-scoped
//!   `&dyn Transport` egress hands it, so the same remote turn is subject to the
//!   privacy boundary (BR-1) and produces a `CostRecord` (BR-2) — the wiring the
//!   router already builds a context for, now actually executed.
//!
//! Both collapse a turn to the same [`SourceTurn`]: the assistant text and one
//! [`TurnDecision`] (call a tool, end the turn, or a malformed call folded back).
//! The loop switches on that and never sees a provider-specific shape.

use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use teton_inference::Engine;
use teton_protocol::{Phase, ProviderId, SessionId};
use teton_providers::{
    Message, Provider, Role, TokenUsage, ToolSpec, Transport, TurnEvent, TurnRequest,
};

use crate::cost::CostAttribution;
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::{ContextManager, Provenance as CtxProvenance, ToolProvenance};
use super::tools::ToolRegistry;
use super::turn_loop::{parse_turn, HarnessConfig, HarnessError, ParsedTurn};

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

/// One model turn produced by a [`CompletionSource`]: the assistant's full text,
/// what it decided to do, and the token usage (populated for remote turns; the
/// local tier is free and reports zero).
#[derive(Debug, Clone)]
pub struct SourceTurn {
    /// The assistant's full text for this turn (may be empty for a pure tool call).
    pub text: String,
    /// The model's decision.
    pub decision: TurnDecision,
    /// Token usage, when the source knows it (remote). `0/0` for the local tier.
    pub usage: TokenUsage,
}

/// A source of model turns for the turn loop: local engine or remote provider.
///
/// `produce_turn` is handed the already-assembled `prompt`, the egress
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
        prompt: &str,
        provenance: &Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError>;
}

/// The local-tier source: drives the [`Engine`] behind a shared `Mutex` and parses
/// its text reply. Transport-free — egress is impossible on this path.
pub struct LocalEngineSource<'a> {
    engine: &'a Mutex<dyn Engine>,
}

impl<'a> LocalEngineSource<'a> {
    /// A source over the shared local `engine`.
    #[must_use]
    pub fn new(engine: &'a Mutex<dyn Engine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl CompletionSource for LocalEngineSource<'_> {
    async fn produce_turn(
        &mut self,
        prompt: &str,
        _provenance: &Provenance,
        config: &HarnessConfig,
        _tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError> {
        // The local engine completes synchronously; its output is atomic, so the
        // turn is emitted as one chunk (unchanged from the local-first loop).
        let completion = {
            let guard = self.engine.lock().expect("engine mutex poisoned");
            guard.complete(prompt, &config.gen_params, &mut |_| {})?
        };
        let text = completion.text;
        on_token(&text);
        let decision = match parse_turn(&text, exposed) {
            ParsedTurn::ToolCall { name, arguments } => TurnDecision::ToolCall { name, arguments },
            ParsedTurn::EndTurn(final_text) => TurnDecision::EndTurn { final_text },
            ParsedTurn::Malformed(reason) => TurnDecision::Malformed { reason },
        };
        Ok(SourceTurn {
            text,
            decision,
            usage: TokenUsage {
                input_tokens: u64::from(completion.prompt_tokens),
                output_tokens: u64::from(completion.completion_tokens),
            },
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
        }
    }

    /// Pin the structured-mode `phase` this source's calls are attributed to
    /// (drives per-phase cost attribution, AC-3/BR-2). Absent in freeform mode.
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = Some(phase);
        self
    }
}

#[async_trait]
impl<T: Transport> CompletionSource for RemoteProviderSource<'_, T> {
    async fn produce_turn(
        &mut self,
        prompt: &str,
        provenance: &Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        _exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError> {
        // The assembled prompt already carries the system instructions and the
        // whole conversation, so it travels as a single user message — the loop
        // owns context assembly; the adapter only maps it to the wire body.
        let request = TurnRequest {
            model: self.model.clone(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: prompt.to_owned(),
            }],
            tools: exposed_tool_specs(tools, config.max_tools),
            max_tokens: config.gen_params.max_tokens,
        };

        // BR-2: attribute the call to (session, phase, model). BR-1: the scoped
        // transport bakes in this turn's provenance so the provider cannot bypass
        // the boundary check.
        let attribution = match self.phase {
            Some(phase) => CostAttribution::new(self.model.clone()).with_phase(phase),
            None => CostAttribution::new(self.model.clone()),
        };
        let egress_ctx = EgressContext::new(self.provider_id.clone())
            .with_session(self.session_id.clone())
            .with_cost(attribution);
        let transport = self.egress.scoped(provenance.clone(), egress_ctx);

        // Errors known at open time (including a privacy block, surfaced as a
        // transport refusal) come back here before any events flow.
        let mut stream = self.provider.stream_turn(request, &transport).await?;

        let mut text = String::new();
        let mut tool_call: Option<TurnDecision> = None;
        let mut usage = TokenUsage::default();
        while let Some(event) = stream.next().await {
            match event? {
                TurnEvent::TextDelta(delta) => {
                    on_token(&delta);
                    text.push_str(&delta);
                }
                // MVP: the reduced harness runs one tool per turn, so the first
                // assembled call wins; later parallel calls this turn are ignored.
                TurnEvent::ToolCall(call) if tool_call.is_none() => {
                    tool_call = Some(TurnDecision::ToolCall {
                        name: call.name,
                        arguments: call.arguments,
                    });
                }
                TurnEvent::ToolCall(_) => {}
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
#[must_use]
pub fn context_provenance(ctx: &ContextManager) -> Provenance {
    let mut prov = Provenance::empty();
    for block in ctx.blocks() {
        if let CtxProvenance::Tool { provenance, .. } = &block.provenance {
            match provenance {
                ToolProvenance::Sources(paths) => {
                    for path in paths {
                        prov.merge(&Provenance::tainted_by(path.clone()));
                    }
                }
                ToolProvenance::Unknown => prov.mark_unknown(),
            }
        }
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
    use teton_inference::MockEngine;

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
        let engine = Mutex::new(MockEngine::with_response(
            "mock",
            r#"{"tool":"read","arguments":{"path":"a.rs"}}"#,
        ));
        let mut source = LocalEngineSource::new(&engine);
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let mut streamed = String::new();
        let turn = source
            .produce_turn(
                "prompt",
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
        let engine = Mutex::new(MockEngine::with_response(
            "mock",
            "All done, nothing more to do.",
        ));
        let mut source = LocalEngineSource::new(&engine);
        let tools = ToolRegistry::with_builtins();
        let exposed = tools.exposed_names(None);
        let turn = source
            .produce_turn(
                "prompt",
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
}
