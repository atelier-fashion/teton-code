//! The agentic turn loop: assemble context → call the model → dispatch a tool →
//! fold the result → repeat, until the model ends its turn or a ceiling is hit.
//!
//! (The file is `turn_loop.rs`, not `loop.rs`, because `loop` is a Rust keyword
//! and `harness::loop` will not parse as a module path.)
//!
//! ## Built for weak models
//!
//! The loop's *native* shape is the degraded one (BR-6), because the product
//! thesis is that a small local model can drive it: **short** ([`HarnessConfig`]
//! defaults to a low `max_turns`), a **small** tool set (capped by `max_tools`),
//! and **mandatory verification** — after an edit the loop refuses to let the
//! model declare victory until it has re-read or tested the change. A strong
//! model is the same loop with a longer leash ([`HarnessConfig::for_strong_model`]).
//!
//! ## Local-first (architecture D-3)
//!
//! This function drives the [`Engine`] trait — the local tier — and nothing else.
//! It takes no [`Transport`](teton_providers::Transport), no provider, no network
//! handle: **egress is impossible here by construction**, which is what makes the
//! offline AC-1 path a zero-egress guarantee rather than a hope. Remote routing
//! (and the egress choke point that enforces BR-1) arrives in TASK-010/TASK-007
//! and plugs in at the [`ProvenanceHook`] seam.
//!
//! ## Termination
//!
//! The loop always terminates: it stops on the model's end-of-turn, and it is
//! hard-capped by `max_turns`. A malformed or hallucinated tool call does not
//! break it — the error is folded back for the model to correct, still under the
//! same turn ceiling — so no sequence of bad model output produces an unbounded
//! loop.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use teton_inference::{Engine, EngineError, GenParams};
use teton_protocol::events::{Event, SessionUpdate, SessionUpdatePayload, ToolCallStatus};
use teton_protocol::methods::StopReason;
use teton_protocol::{ProviderId, SessionId};
use teton_providers::{HarnessProfile, ToolCall};

use crate::broadcast::EventBus;

use super::context::{summarize_if_large, ContextManager, ProvenanceHook};
use super::permissions::{PermissionDecision, PermissionGate};
use super::tools::{ToolContext, ToolRegistry};

/// Tools that count as a verification step after an edit.
const VERIFY_TOOLS: &[&str] = &["shell", "read", "grep"];

/// A failure the loop cannot fold back to the model.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// The local engine could not serve (unavailable or backend error). The
    /// router (a later task) turns this into a remote fallback; here it ends the
    /// local turn.
    #[error("local engine error: {0}")]
    Engine(#[from] EngineError),
}

/// Tuning for the loop. The [`Default`] is the weak-model profile (BR-6): short
/// loop, verification required.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Hard ceiling on model calls in one turn.
    pub max_turns: u32,
    /// Token budget for the assembled context.
    pub context_budget_tokens: usize,
    /// Tool results larger than this (in approx tokens) are summarized locally.
    pub summarize_threshold_tokens: usize,
    /// Cap on tools exposed to the model (`None` = all).
    pub max_tools: Option<u32>,
    /// Require a verification step after an edit before the turn may end.
    pub require_verification: bool,
    /// Generation parameters passed to the engine.
    pub gen_params: GenParams,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        // The weak-model native shape.
        Self {
            max_turns: 12,
            context_budget_tokens: 4_096,
            summarize_threshold_tokens: 1_500,
            max_tools: Some(5),
            require_verification: true,
            gen_params: GenParams::default(),
        }
    }
}

impl HarnessConfig {
    /// A longer leash for a reliable tool-caller: more turns, no verification
    /// gate, full tool set. Same loop, weaker constraints.
    #[must_use]
    pub fn for_strong_model() -> Self {
        Self {
            max_turns: 40,
            max_tools: None,
            require_verification: false,
            ..Self::default()
        }
    }

    /// Derive a config from a provider's BR-6 [`HarnessProfile`], so a degraded
    /// remote provider runs the same reduced loop the local tier runs natively.
    #[must_use]
    pub fn from_harness_profile(profile: HarnessProfile) -> Self {
        Self {
            max_turns: profile.max_tool_iterations.max(1),
            max_tools: profile.max_tools,
            require_verification: profile.require_verification,
            ..Self::default()
        }
    }
}

/// A per-turn routing input from the router (TASK-010): which provider serves
/// the turn and the harness profile it runs under.
///
/// This is the seam by which the router hands the loop a **provider + profile per
/// turn** — the BR-6 degradation decision — without touching the local-first
/// [`run_session_turn`] signature. The offline AC-1 path stays a transport-free,
/// zero-egress call; a routed turn wraps it with [`run_routed_session_turn`],
/// which runs the same loop under [`TurnRoute::config`]. The remote wiring proper
/// (privacy + cost) lives at the egress choke point the router builds a context
/// for; the loop's job here is only to run at the right profile and to know which
/// provider the turn is attributed to.
#[derive(Debug, Clone)]
pub struct TurnRoute {
    /// Provider selected for this turn (attribution; feeds `route_decided` /
    /// `cost_recorded` above this layer).
    pub provider_id: ProviderId,
    /// Concrete model chosen, when known.
    pub model: Option<String>,
    /// Harness configuration for this turn — the BR-6 profile the router derived
    /// from the selected provider's capabilities.
    pub config: HarnessConfig,
}

impl TurnRoute {
    /// A route naming `provider_id` and running under `config`, with no model.
    #[must_use]
    pub fn new(provider_id: impl Into<ProviderId>, config: HarnessConfig) -> Self {
        Self {
            provider_id: provider_id.into(),
            model: None,
            config,
        }
    }

    /// Set the concrete model for this turn.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// The result of running one prompt turn to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Why the turn ended.
    pub stop_reason: StopReason,
    /// Number of model calls made.
    pub turns: u32,
    /// The model's final plain-text answer (empty if it hit a ceiling).
    pub final_text: String,
    /// Whether at least one edit landed this turn.
    pub edited: bool,
    /// Whether an edit was followed by a verification step.
    pub verified: bool,
}

/// Publishes `session_update` events for one session (streaming turn surface,
/// ACP `session/update`). Shares TASK-004's [`EventBus`] with the permission
/// gate.
pub struct SessionEvents {
    bus: Arc<EventBus>,
    session_id: SessionId,
}

impl SessionEvents {
    /// Session-scoped event emitter over `bus`.
    #[must_use]
    pub fn new(bus: Arc<EventBus>, session_id: SessionId) -> Self {
        Self { bus, session_id }
    }

    fn emit(&self, update: SessionUpdatePayload) {
        self.bus.publish(
            Some(self.session_id.clone()),
            Event::SessionUpdate(SessionUpdate { update }),
        );
    }

    fn agent_message(&self, text: &str) {
        self.emit(SessionUpdatePayload::AgentMessageChunk {
            text: text.to_owned(),
        });
    }

    fn tool_started(&self, id: &str, title: &str) {
        self.emit(SessionUpdatePayload::ToolCall {
            tool_call_id: id.to_owned(),
            title: title.to_owned(),
            status: ToolCallStatus::InProgress,
        });
    }

    fn tool_finished(&self, id: &str, ok: bool) {
        self.emit(SessionUpdatePayload::ToolCallUpdate {
            tool_call_id: id.to_owned(),
            status: if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            },
        });
    }
}

/// Drive one prompt turn to completion against the local engine.
///
/// `ctx` must already hold the system prompt and the user's prompt (see
/// [`build_system_prompt`]). The loop appends the model's turns and tool results
/// to it as it runs.
///
/// # Errors
/// Returns [`HarnessError::Engine`] if the local engine cannot serve. Tool
/// failures and malformed model output are *not* errors — they are folded back
/// into the context for the model to handle.
///
/// # Blocking
/// Tool dispatch (notably `shell`) runs synchronously; a production caller on a
/// multi-thread runtime should wrap this in `spawn_blocking` for the tool phase.
#[allow(clippy::too_many_arguments)]
pub async fn run_session_turn(
    engine: &Mutex<dyn Engine>,
    tools: &ToolRegistry,
    tool_ctx: &ToolContext,
    gate: &PermissionGate,
    events: &SessionEvents,
    ctx: &mut ContextManager,
    config: &HarnessConfig,
    hook: &mut dyn ProvenanceHook,
) -> Result<TurnOutcome, HarnessError> {
    let exposed = tools.exposed_names(config.max_tools);
    let mut turns = 0u32;
    let mut edited = false;
    let mut verified = false;
    let mut nudged = false;

    loop {
        if turns >= config.max_turns {
            return Ok(TurnOutcome {
                stop_reason: StopReason::MaxTurnRequests,
                turns,
                final_text: String::new(),
                edited,
                verified,
            });
        }

        // ---- model call (engine lock is never held across an await) ----
        let prompt = ctx.assemble(hook);
        let text = {
            let guard = engine.lock().expect("engine mutex poisoned");
            guard
                .complete(&prompt, &config.gen_params, &mut |_| {})?
                .text
        };
        turns += 1;
        events.agent_message(&text);

        match parse_turn(&text, &exposed) {
            ParsedTurn::EndTurn(final_text) => {
                // Mandatory verification (BR-6): a weak model may not declare an
                // edit done without checking it. Nudge once, then respect the
                // model's decision so the loop still terminates.
                if config.require_verification && edited && !verified && !nudged {
                    nudged = true;
                    ctx.push_model(text.clone());
                    ctx.push_tool_result(
                        "system",
                        None,
                        "You edited a file but have not verified the change. Run a \
                         verification step (re-read the file, or run a build/test with \
                         the shell tool) and confirm the result before finishing.",
                    );
                    continue;
                }
                ctx.push_model(final_text.clone());
                return Ok(TurnOutcome {
                    stop_reason: StopReason::EndTurn,
                    turns,
                    final_text,
                    edited,
                    verified,
                });
            }

            ParsedTurn::Malformed(reason) => {
                // A hallucinated tool or bad arguments: correct the model and keep
                // going, still bounded by max_turns (no unbounded loop).
                ctx.push_model(text.clone());
                ctx.push_tool_result(
                    "system",
                    None,
                    format!(
                        "That was not a valid tool call: {reason}. Reply with a single \
                         JSON object {{\"tool\":\"<name>\",\"arguments\":{{...}}}} using \
                         one of these tools: {}. Or give a plain-text final answer.",
                        exposed.join(", ")
                    ),
                );
                continue;
            }

            ParsedTurn::ToolCall { name, arguments } => {
                ctx.push_model(text.clone());
                let call = ToolCall {
                    id: format!("call-{turns}"),
                    name: name.clone(),
                    arguments: arguments.clone(),
                };
                let title = describe_call(&call);
                events.tool_started(&call.id, &title);

                match gate.authorize(&name, Some(title)).await {
                    PermissionDecision::Denied => {
                        events.tool_finished(&call.id, false);
                        ctx.push_tool_result(
                            name.clone(),
                            None,
                            format!(
                                "Permission denied: the user declined `{name}`. Do not \
                                 retry this tool; take a different approach or finish."
                            ),
                        );
                        continue;
                    }
                    PermissionDecision::Allowed => {
                        let outcome = tools.dispatch(&name, tool_ctx, &arguments);
                        events.tool_finished(&call.id, !outcome.is_error);

                        if name == "edit" && !outcome.is_error {
                            edited = true;
                            verified = false;
                        }
                        if edited && VERIFY_TOOLS.contains(&name.as_str()) {
                            verified = true;
                        }

                        let folded = if outcome.is_error {
                            format!("ERROR: {}", outcome.content)
                        } else {
                            outcome.content
                        };
                        let folded = summarize_if_large(
                            engine,
                            &name,
                            &folded,
                            config.summarize_threshold_tokens,
                        );
                        ctx.push_tool_result(name, path_arg(&arguments), folded);
                        ctx.truncate_to_budget();
                        continue;
                    }
                }
            }
        }
    }
}

/// Drive one prompt turn under an explicit [`TurnRoute`] chosen by the router
/// (TASK-010).
///
/// A thin, additive wrapper over [`run_session_turn`]: it runs the same loop
/// under the route's degradation-derived [`HarnessConfig`] (BR-6), so a turn
/// routed to a weak tool-caller runs the reduced profile and a turn routed to a
/// reliable one runs the full loop — from a single per-turn decision. The
/// `engine` still serves the tokens and the local-only [`run_session_turn`] path
/// is unchanged; the route names the provider the turn is attributed to and pins
/// its profile. Remote privacy/cost enforcement is applied at the egress choke
/// point the router builds a context for, not here.
///
/// # Errors
/// Propagates [`HarnessError`] from the underlying [`run_session_turn`].
#[allow(clippy::too_many_arguments)]
pub async fn run_routed_session_turn(
    engine: &Mutex<dyn Engine>,
    tools: &ToolRegistry,
    tool_ctx: &ToolContext,
    gate: &PermissionGate,
    events: &SessionEvents,
    ctx: &mut ContextManager,
    route: &TurnRoute,
    hook: &mut dyn ProvenanceHook,
) -> Result<TurnOutcome, HarnessError> {
    run_session_turn(
        engine,
        tools,
        tool_ctx,
        gate,
        events,
        ctx,
        &route.config,
        hook,
    )
    .await
}

/// Build the system prompt: the agent's instructions plus the exposed tool docs
/// and the tool-call format the local model must follow.
#[must_use]
pub fn build_system_prompt(tools: &ToolRegistry, config: &HarnessConfig) -> String {
    let mut s = String::from(
        "You are Teton Code, a coding agent that reads, edits, and verifies files \
         using tools.\n\
         Work in short steps and use exactly one tool per reply.\n\
         To call a tool, reply with ONLY a JSON object on its own:\n\
         {\"tool\": \"<name>\", \"arguments\": { ... }}\n\
         When the task is complete, reply with a short plain-text summary and NO JSON.\n",
    );
    if config.require_verification {
        s.push_str(
            "After any edit you MUST verify it (re-read the file, or run a build/test \
             with the shell tool) before finishing.\n",
        );
    }
    s.push_str("\nAvailable tools:\n");
    s.push_str(&tools.docs(config.max_tools));
    s
}

/// What the loop parsed out of one model reply.
#[derive(Debug, Clone, PartialEq)]
enum ParsedTurn {
    /// A well-formed call to a known tool.
    ToolCall {
        /// Tool name.
        name: String,
        /// Argument object.
        arguments: Value,
    },
    /// No tool call — the model's final answer.
    EndTurn(String),
    /// Something tool-call-shaped but invalid (unknown tool, non-object args).
    Malformed(String),
}

/// Parse a model reply into a tool call, an end-of-turn answer, or a malformed
/// call. A reply is a tool call only if it contains a JSON object with a `tool`
/// (or `name`) key; anything else is treated as the final answer.
fn parse_turn(text: &str, known_tools: &[&str]) -> ParsedTurn {
    for candidate in json_object_candidates(text) {
        let Ok(value) = serde_json::from_str::<Value>(&candidate) else {
            continue;
        };
        let name = value
            .get("tool")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str);
        let Some(name) = name else {
            // JSON without a tool key: not a tool call. Keep scanning in case a
            // real call follows; if none is found this becomes an end-of-turn.
            continue;
        };

        let arguments = value
            .get("arguments")
            .or_else(|| value.get("input"))
            .or_else(|| value.get("args"))
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        if !known_tools.contains(&name) {
            return ParsedTurn::Malformed(format!("`{name}` is not an available tool"));
        }
        if !arguments.is_object() {
            return ParsedTurn::Malformed(format!("arguments for `{name}` must be a JSON object"));
        }
        return ParsedTurn::ToolCall {
            name: name.to_owned(),
            arguments,
        };
    }
    ParsedTurn::EndTurn(text.trim().to_owned())
}

/// Extract every top-level `{...}` object substring, ignoring braces inside JSON
/// strings. Robust to prose or code fences around the object.
fn json_object_candidates(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    // Brace boundaries are ASCII, so the slice is UTF-8 safe.
                    out.push(text[start..=i].to_owned());
                }
            }
            _ => {}
        }
    }
    out
}

/// A short human title for a tool call (drives the `tool_call` event title).
fn describe_call(call: &ToolCall) -> String {
    match call.name.as_str() {
        "read" | "edit" => path_arg(&call.arguments)
            .map(|p| format!("{} {p}", call.name))
            .unwrap_or_else(|| call.name.clone()),
        "shell" => call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|c| format!("shell: {c}"))
            .unwrap_or_else(|| "shell".to_owned()),
        "grep" | "glob" => call
            .arguments
            .get("pattern")
            .and_then(Value::as_str)
            .map(|p| format!("{} {p}", call.name))
            .unwrap_or_else(|| call.name.clone()),
        other => other.to_owned(),
    }
}

/// The `path` argument as an owned string, when present.
fn path_arg(arguments: &Value) -> Option<String> {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_tool_call() {
        let parsed = parse_turn(
            r#"{"tool":"read","arguments":{"path":"a.rs"}}"#,
            &["read", "edit"],
        );
        assert_eq!(
            parsed,
            ParsedTurn::ToolCall {
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": "a.rs" }),
            }
        );
    }

    #[test]
    fn parses_a_fenced_tool_call_with_prose() {
        let text = "I'll read the file.\n```json\n{\"tool\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```";
        let parsed = parse_turn(text, &["read"]);
        assert!(matches!(parsed, ParsedTurn::ToolCall { .. }));
    }

    #[test]
    fn plain_text_is_end_of_turn() {
        let parsed = parse_turn("All done. The file now returns 2.", &["read"]);
        assert_eq!(
            parsed,
            ParsedTurn::EndTurn("All done. The file now returns 2.".to_owned())
        );
    }

    #[test]
    fn unknown_tool_is_malformed_not_end_of_turn() {
        let parsed = parse_turn(r#"{"tool":"delete_everything","arguments":{}}"#, &["read"]);
        assert!(matches!(parsed, ParsedTurn::Malformed(_)));
    }

    #[test]
    fn non_object_arguments_are_malformed() {
        let parsed = parse_turn(r#"{"tool":"read","arguments":"a.rs"}"#, &["read"]);
        assert!(matches!(parsed, ParsedTurn::Malformed(_)));
    }

    #[test]
    fn braces_inside_strings_do_not_break_scanning() {
        let cands = json_object_candidates(r#"{"tool":"grep","arguments":{"pattern":"a}b{c"}}"#);
        assert_eq!(cands.len(), 1);
        assert!(serde_json::from_str::<Value>(&cands[0]).is_ok());
    }

    #[test]
    fn turn_route_carries_provider_model_and_profile() {
        let route = TurnRoute::new("deepseek", HarnessConfig::for_strong_model())
            .with_model("deepseek-chat");
        assert_eq!(route.provider_id, ProviderId::from("deepseek"));
        assert_eq!(route.model.as_deref(), Some("deepseek-chat"));
        // The profile is carried verbatim — the loop runs under exactly it.
        assert_eq!(route.config.max_turns, 40);
        assert!(route.model.is_some());
    }

    #[test]
    fn from_harness_profile_maps_degraded_to_a_short_verified_loop() {
        use teton_core::ToolCallTier;
        use teton_providers::CapabilityProfile;
        let profile = CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: true,
            max_context: 32_000,
        }
        .harness_profile();
        let config = HarnessConfig::from_harness_profile(profile);
        assert!(config.require_verification);
        assert!(config.max_turns <= 12);
        assert_eq!(config.max_tools, Some(5));
    }
}
