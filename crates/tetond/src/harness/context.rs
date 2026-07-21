//! Context-window management for small models.
//!
//! Weak models have small, precious context windows, so this module does two
//! things aggressively:
//!
//! 1. **Truncation** — the conversation is kept under a token budget by dropping
//!    the oldest turns first (the system prompt and the most recent turns are
//!    preserved), with a one-line marker so the model knows history was elided.
//! 2. **Tool-result summarization** — a large tool result (a long file, a noisy
//!    build log) is condensed by the *local* engine before it enters context, via
//!    [`summarize_if_large`], so a single grep can't evict the whole conversation.
//!
//! Every context block carries a [`Provenance`] tag. That tag is the seam
//! TASK-007's egress choke point plugs into: a [`ProvenanceHook`] is invoked for
//! each block as the prompt is assembled, so egress can identify
//! boundary-protected content (BR-1) before anything goes remote. On the
//! local-only path the hook is a no-op ([`NoopProvenanceHook`]) — there is no
//! egress to guard.

use std::collections::BTreeSet;
use std::sync::Mutex;

use teton_inference::{Engine, GenParams};

/// The egress provenance of a tool result — the files a tool actually touched,
/// or an explicit "cannot tell" state (REQ-544 C-1).
///
/// This is what makes BR-1 enforcement honest for tools beyond `read`: a tool
/// reports the repo-relative paths it read/enumerated ([`ToolProvenance::Sources`]),
/// or, when its touched files are unknowable (a `shell` command runs arbitrary
/// code), it reports [`ToolProvenance::Unknown`], which egress fail-closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProvenance {
    /// The tool surfaced content derived from these repo-relative paths. An empty
    /// set means it touched no repo file (a pure computation, a benign status).
    Sources(BTreeSet<String>),
    /// The tool's touched files cannot be determined (e.g. `shell`): fail-closed
    /// at egress whenever any boundary is configured.
    Unknown,
}

impl ToolProvenance {
    /// No file provenance — content from no repo file.
    #[must_use]
    pub fn none() -> Self {
        ToolProvenance::Sources(BTreeSet::new())
    }

    /// Provenance for a single touched `path`.
    #[must_use]
    pub fn path(path: impl Into<String>) -> Self {
        let mut set = BTreeSet::new();
        set.insert(path.into());
        ToolProvenance::Sources(set)
    }

    /// Provenance for a set of touched paths.
    #[must_use]
    pub fn paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ToolProvenance::Sources(paths.into_iter().map(Into::into).collect())
    }
}

/// Where a piece of context came from — the basis for egress provenance tagging
/// (BR-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The daemon's own instructions.
    System,
    /// End-user prompt text.
    User,
    /// Model-generated text (assistant turn).
    Model,
    /// A tool result, tagged with the [`ToolProvenance`] of the files the tool
    /// touched so egress can match them against a privacy boundary (or
    /// fail-close on an unknown-provenance result).
    Tool {
        /// Tool that produced the result.
        tool: String,
        /// The files the tool touched (or `Unknown`).
        provenance: ToolProvenance,
    },
}

/// The speaker role of a context block, for prompt rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRole {
    /// User input.
    User,
    /// Assistant output.
    Assistant,
    /// A tool result fed back to the model.
    Tool,
}

impl BlockRole {
    fn label(self) -> &'static str {
        match self {
            BlockRole::User => "User",
            BlockRole::Assistant => "Assistant",
            BlockRole::Tool => "Tool",
        }
    }
}

/// The speaker role of a [`StructuredMessage`] (REQ-544 M-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// A user turn: the end-user prompt, or a tool result fed back. The text
    /// harness has no provider `tool_call_id` protocol, so tool results ride as
    /// user content — the shape Anthropic folds tool results into anyway, and the
    /// only one an OpenAI-compatible endpoint accepts without a preceding
    /// assistant `tool_calls` entry.
    User,
    /// A prior assistant turn.
    Assistant,
}

/// One role-typed message in the structured (chat) rendering of the context
/// (REQ-544 M-8): the shape a remote provider actually wants, as opposed to one
/// flattened user blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredMessage {
    /// Speaker role.
    pub role: MessageRole,
    /// Message text.
    pub text: String,
}

/// The assembled context in both shapes the completion sources consume
/// (REQ-544 M-8): a flat string for the local text engine, plus a system prompt
/// and alternating user/assistant messages for a remote chat provider.
///
/// A remote turn maps [`Self::system`] to `TurnRequest.system` and
/// [`Self::messages`] to `TurnRequest.messages`, so it sends a real system field
/// and role-typed turns rather than concatenating everything into one
/// `Role::User` message (which degrades tool-calling and defeats prompt caching).
#[derive(Debug, Clone)]
pub struct PreparedPrompt {
    /// Flat single-string rendering for a local text engine.
    pub flat: String,
    /// Top-level system prompt for a remote provider (non-empty whenever the
    /// context carries a system prompt).
    pub system: String,
    /// The conversation as alternating user/assistant messages, starting with a
    /// user turn.
    pub messages: Vec<StructuredMessage>,
}

/// One block of conversation context with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    /// Rendering role.
    pub role: BlockRole,
    /// Block text.
    pub text: String,
    /// Where the text originated (egress tagging seam).
    pub provenance: Provenance,
}

/// A hook invoked for each context block as a prompt is assembled.
///
/// This is the extension point for TASK-007's egress choke point: before content
/// is sent to a remote provider, egress inspects each block's [`Provenance`] to
/// enforce privacy boundaries (BR-1). The local path passes a
/// [`NoopProvenanceHook`].
pub trait ProvenanceHook: Send {
    /// Called once per block, in prompt order.
    fn on_block(&mut self, block: &ContextBlock);
}

/// A [`ProvenanceHook`] that does nothing — the local-only path, where there is
/// no egress to guard.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProvenanceHook;

impl ProvenanceHook for NoopProvenanceHook {
    fn on_block(&mut self, _block: &ContextBlock) {}
}

/// A [`ProvenanceHook`] that records the provenance of every block it sees.
///
/// Used by tests to assert what would have been eligible for egress (and, on the
/// local path, that nothing carried a remote destination).
#[derive(Debug, Default)]
pub struct RecordingProvenanceHook {
    /// Provenance of each block seen, in order.
    pub seen: Vec<Provenance>,
}

impl ProvenanceHook for RecordingProvenanceHook {
    fn on_block(&mut self, block: &ContextBlock) {
        self.seen.push(block.provenance.clone());
    }
}

/// Manages the assembled context for one session under a token budget.
#[derive(Debug, Clone)]
pub struct ContextManager {
    system: String,
    blocks: Vec<ContextBlock>,
    budget_tokens: usize,
    truncated: bool,
}

impl ContextManager {
    /// A manager with the given system prompt and token budget.
    #[must_use]
    pub fn new(system: impl Into<String>, budget_tokens: usize) -> Self {
        Self {
            system: system.into(),
            blocks: Vec::new(),
            budget_tokens,
            truncated: false,
        }
    }

    /// Append a user turn.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(ContextBlock {
            role: BlockRole::User,
            text: text.into(),
            provenance: Provenance::User,
        });
    }

    /// Append an assistant turn.
    pub fn push_model(&mut self, text: impl Into<String>) {
        self.blocks.push(ContextBlock {
            role: BlockRole::Assistant,
            text: text.into(),
            provenance: Provenance::Model,
        });
    }

    /// Append a tool result, tagged with the tool and (optionally) the single
    /// file it concerns. A convenience over [`ContextManager::push_tool_result_prov`]:
    /// `None` → no file provenance, `Some(p)` → the single touched path `p`.
    pub fn push_tool_result(
        &mut self,
        tool: impl Into<String>,
        path: Option<String>,
        text: impl Into<String>,
    ) {
        let provenance = match path {
            Some(p) => ToolProvenance::path(p),
            None => ToolProvenance::none(),
        };
        self.push_tool_result_prov(tool, provenance, text);
    }

    /// Append a tool result tagged with its full [`ToolProvenance`] — the set of
    /// files the tool touched, or [`ToolProvenance::Unknown`] (REQ-544 C-1). This
    /// is the loop's tagging path: a `shell` result folds in as `Unknown`, a
    /// `grep`/`glob`/MCP result as the set of files it surfaced.
    pub fn push_tool_result_prov(
        &mut self,
        tool: impl Into<String>,
        provenance: ToolProvenance,
        text: impl Into<String>,
    ) {
        let tool = tool.into();
        self.blocks.push(ContextBlock {
            role: BlockRole::Tool,
            text: text.into(),
            provenance: Provenance::Tool { tool, provenance },
        });
    }

    /// The blocks currently held.
    #[must_use]
    pub fn blocks(&self) -> &[ContextBlock] {
        &self.blocks
    }

    /// Estimated total tokens (system + all blocks), by a whitespace heuristic
    /// consistent with the mock engine's counting.
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        let mut n = approx_tokens(&self.system);
        for b in &self.blocks {
            n += approx_tokens(&b.text);
        }
        n
    }

    /// Drop the oldest blocks until the estimate fits the budget. The system
    /// prompt and the single most recent block are always preserved.
    pub fn truncate_to_budget(&mut self) {
        while self.estimated_tokens() > self.budget_tokens && self.blocks.len() > 1 {
            self.blocks.remove(0);
            self.truncated = true;
        }
    }

    /// Whether any history has been dropped by truncation.
    #[must_use]
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    /// Render the full prompt string for a text engine, invoking `hook` for the
    /// system block and every conversation block (egress seam).
    #[must_use]
    pub fn assemble(&self, hook: &mut dyn ProvenanceHook) -> String {
        hook.on_block(&ContextBlock {
            role: BlockRole::User, // role unused for the system block
            text: self.system.clone(),
            provenance: Provenance::System,
        });

        let mut out = String::new();
        out.push_str(&self.system);
        out.push_str("\n\n");
        if self.truncated {
            out.push_str("[earlier conversation truncated to fit the context window]\n\n");
        }
        for block in &self.blocks {
            hook.on_block(block);
            out.push_str(block.role.label());
            if let Provenance::Tool { tool, .. } = &block.provenance {
                out.push_str(&format!(" ({tool})"));
            }
            out.push_str(":\n");
            out.push_str(&block.text);
            out.push_str("\n\n");
        }
        out.push_str("Assistant:\n");
        out
    }

    /// Render the assembled context in **both** shapes the completion sources need
    /// (REQ-544 M-8): the flat single-string form for a local text engine, and a
    /// system prompt plus role-typed messages for a remote chat provider.
    ///
    /// The `hook` is invoked for the system block and every conversation block
    /// exactly as [`ContextManager::assemble`] does (the egress seam) — `prepare`
    /// delegates the flat rendering to it, so provenance tagging is unchanged.
    ///
    /// Tool results are carried as `User` turns and consecutive same-role blocks
    /// are merged, so the messages always alternate user/assistant starting with a
    /// user turn — the shape Anthropic requires and every OpenAI-compatible
    /// endpoint accepts. This replaces the single-`User`-blob request that
    /// collapsed system, history, and tool results together.
    #[must_use]
    pub fn prepare(&self, hook: &mut dyn ProvenanceHook) -> PreparedPrompt {
        // Reuse assemble for the flat rendering AND the hook invocations, so the
        // egress seam sees exactly the same blocks it always has.
        let flat = self.assemble(hook);

        let mut system = self.system.clone();
        if self.truncated {
            system.push_str("\n\n[earlier conversation was truncated to fit the context window]");
        }

        let mut messages: Vec<StructuredMessage> = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            let role = match block.role {
                BlockRole::Assistant => MessageRole::Assistant,
                BlockRole::User | BlockRole::Tool => MessageRole::User,
            };
            // Preserve the "(tool)" annotation the flat form carries so the model
            // can still tell a tool result from a genuine user turn.
            let text = match &block.provenance {
                Provenance::Tool { tool, .. } => format!("[{tool} tool result]\n{}", block.text),
                _ => block.text.clone(),
            };
            // Merge into the previous message when the role repeats, guaranteeing
            // strict user/assistant alternation regardless of block order.
            if let Some(last) = messages.last_mut() {
                if last.role == role {
                    last.text.push_str("\n\n");
                    last.text.push_str(&text);
                    continue;
                }
            }
            messages.push(StructuredMessage { role, text });
        }

        // REQ-544 M-8: guarantee the sequence is non-empty and starts with a user
        // turn. Truncation can evict the oldest user turn(s), leaving an assistant
        // turn first (which alternation-merging cannot fix — there is nothing
        // before it to merge into); an empty context yields no messages at all.
        // Either would make a remote request start with role "assistant" or carry
        // an empty `messages` array — both are hard Anthropic 400s. Prepend a
        // single synthetic user turn when needed; the surviving assistant content
        // is preserved, and alternation still holds afterward.
        let needs_leading_user = messages.first().is_none_or(|m| m.role != MessageRole::User);
        if needs_leading_user {
            messages.insert(
                0,
                StructuredMessage {
                    role: MessageRole::User,
                    text: CONTINUATION_USER_TURN.to_owned(),
                },
            );
        }

        PreparedPrompt {
            flat,
            system,
            messages,
        }
    }
}

/// The synthetic leading user turn injected when the structured messages would
/// otherwise be empty or start with an assistant turn (REQ-544 M-8).
///
/// Anthropic (and, less strictly, OpenAI-compatible endpoints) reject a request
/// whose `messages` are empty or do not begin with a `user` turn. Truncation can
/// evict the oldest user turn and leave an assistant turn first; a context with no
/// blocks at all yields no messages. Prepending this turn makes the sequence valid
/// in both cases **without discarding** the surviving assistant content.
const CONTINUATION_USER_TURN: &str =
    "Continue from the conversation so far (earlier turns may have been truncated).";

/// Approximate token count by whitespace splitting (matches the mock engine's
/// prompt-token heuristic, so budgets are consistent end to end).
#[must_use]
pub fn approx_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Summarize a tool result with the local engine when it is larger than
/// `threshold_tokens`; otherwise return it unchanged.
///
/// This keeps a large file read or a noisy log from evicting the conversation on
/// a small model. Summarization is a *local* duty (BR-8 latency, not
/// intelligence): the engine passed here is the local tier. On any engine error
/// the original text is returned (summarization is best-effort, never fatal).
#[must_use]
pub fn summarize_if_large(
    engine: &Mutex<dyn Engine>,
    tool: &str,
    text: &str,
    threshold_tokens: usize,
) -> String {
    if approx_tokens(text) <= threshold_tokens {
        return text.to_owned();
    }
    let prompt = format!(
        "Summarize the following `{tool}` tool output in a few lines, preserving \
         file paths, symbol names, and any errors. Output only the summary.\n\n{text}"
    );
    let params = GenParams::default();
    let guard = engine.lock().expect("engine mutex poisoned");
    match guard.complete(&prompt, &params, &mut |_| {}) {
        Ok(completion) => format!(
            "[summarized {tool} output — {} tokens elided]\n{}",
            approx_tokens(text),
            completion.text
        ),
        Err(_) => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_inference::MockEngine;

    #[test]
    fn assemble_renders_system_and_blocks_and_invokes_hook() {
        let mut ctx = ContextManager::new("SYSTEM", 10_000);
        ctx.push_user("hello");
        ctx.push_model("{\"tool\":\"read\"}");
        ctx.push_tool_result("read", Some("a.rs".to_owned()), "file body");

        let mut hook = RecordingProvenanceHook::default();
        let prompt = ctx.assemble(&mut hook);

        assert!(prompt.starts_with("SYSTEM"));
        assert!(prompt.contains("User:\nhello"));
        assert!(prompt.contains("Tool (read):\nfile body"));
        assert!(prompt.trim_end().ends_with("Assistant:"));

        // System + user + model + tool = 4 blocks observed by the hook.
        assert_eq!(hook.seen.len(), 4);
        assert_eq!(hook.seen[0], Provenance::System);
        assert_eq!(
            hook.seen[3],
            Provenance::Tool {
                tool: "read".to_owned(),
                provenance: ToolProvenance::path("a.rs"),
            }
        );
    }

    #[test]
    fn push_tool_result_prov_carries_unknown_provenance() {
        // REQ-544 C-1: a shell-shaped result folds in as Unknown, distinct from
        // the "no sources" state a benign result carries.
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_tool_result_prov("shell", ToolProvenance::Unknown, "ran a command");
        match &ctx.blocks()[0].provenance {
            Provenance::Tool { tool, provenance } => {
                assert_eq!(tool, "shell");
                assert_eq!(provenance, &ToolProvenance::Unknown);
            }
            other => panic!("expected a tool block, got {other:?}"),
        }
    }

    #[test]
    fn truncation_drops_oldest_and_marks_it() {
        // Tiny budget forces eviction.
        let mut ctx = ContextManager::new("sys", 5);
        for i in 0..20 {
            ctx.push_user(format!("message number {i} with several words"));
        }
        ctx.truncate_to_budget();
        assert!(ctx.was_truncated());
        assert!(ctx.blocks().len() < 20);
        let mut hook = NoopProvenanceHook;
        assert!(ctx.assemble(&mut hook).contains("truncated"));
    }

    #[test]
    fn prepare_guarantees_a_leading_user_turn_when_first_block_is_assistant() {
        // REQ-544 M-8 regression: after truncation the oldest surviving block can be
        // an assistant turn. `prepare` must still emit messages that START with a
        // user turn (else Anthropic 400s: "first message must use the 'user' role"),
        // and it must preserve — not discard — the surviving assistant content.
        let mut ctx = ContextManager::new("SYS", 10_000);
        ctx.push_model("assistant speaks first");
        ctx.push_user("then the user replies");

        let mut hook = NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook);

        assert_eq!(prepared.messages.first().unwrap().role, MessageRole::User);
        // The assistant content survives (was prepended-to, not dropped).
        assert!(
            prepared
                .messages
                .iter()
                .any(|m| m.role == MessageRole::Assistant
                    && m.text.contains("assistant speaks first"))
        );
        // Alternation still holds after the synthetic prepend.
        for pair in prepared.messages.windows(2) {
            assert_ne!(pair[0].role, pair[1].role, "roles must alternate");
        }
    }

    #[test]
    fn a_truncated_context_whose_oldest_survivor_is_assistant_still_starts_with_user() {
        // Drive the leading-assistant state through real truncation: a tiny budget
        // evicts the oldest (user) block, leaving an assistant block first.
        let mut ctx = ContextManager::new("s", 8);
        ctx.push_user("aaa aaa aaa aaa aaa"); // 5 tokens — the oldest, evicted first
        ctx.push_model("bbb bbb bbb bbb bbb"); // 5 tokens
        ctx.push_user("ccc"); // 1 token — most recent, always preserved
        ctx.truncate_to_budget();

        assert!(ctx.was_truncated());
        assert_eq!(
            ctx.blocks().first().unwrap().role,
            BlockRole::Assistant,
            "the oldest surviving block must be the assistant turn for this regression"
        );

        let mut hook = NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook);
        assert_eq!(
            prepared.messages.first().unwrap().role,
            MessageRole::User,
            "a truncated context whose oldest survivor is assistant must still lead with user"
        );
    }

    #[test]
    fn prepare_never_yields_empty_messages() {
        // REQ-544 M-8: an empty-ish context (no conversation blocks) must still
        // produce a non-empty user message — Anthropic 400s on an empty `messages`.
        let ctx = ContextManager::new("SYS", 10_000);
        let mut hook = NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook);

        assert_eq!(prepared.messages.len(), 1);
        assert_eq!(prepared.messages[0].role, MessageRole::User);
        assert!(
            !prepared.messages[0].text.is_empty(),
            "the synthetic leading user turn must be non-empty"
        );
    }

    #[test]
    fn prepare_leaves_the_flat_rendering_unchanged_by_the_leading_user_guard() {
        // The local `flat` path must be identical to `assemble`'s output regardless
        // of the structured-messages leading-role fixup (REQ-544 M-8).
        let mut ctx = ContextManager::new("SYS", 10_000);
        ctx.push_model("assistant first");
        ctx.push_user("user second");

        let mut hook_assemble = NoopProvenanceHook;
        let flat_direct = ctx.assemble(&mut hook_assemble);
        let mut hook_prepare = NoopProvenanceHook;
        let prepared = ctx.prepare(&mut hook_prepare);
        assert_eq!(
            prepared.flat, flat_direct,
            "flat rendering must be untouched"
        );
    }

    #[test]
    fn small_tool_results_are_not_summarized() {
        let engine = Mutex::new(MockEngine::new("mock"));
        let out = summarize_if_large(&engine, "read", "short output", 100);
        assert_eq!(out, "short output");
    }

    #[test]
    fn large_tool_results_are_summarized_by_the_local_engine() {
        let engine = Mutex::new(MockEngine::with_response("mock-3b", "CONDENSED"));
        let big = "word ".repeat(500);
        let out = summarize_if_large(&engine, "grep", &big, 50);
        assert!(out.contains("summarized grep output"));
        assert!(out.contains("CONDENSED"));
    }
}
