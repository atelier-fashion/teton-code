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

use std::sync::Mutex;

use teton_inference::{Engine, GenParams};

/// Where a piece of context came from — the basis for egress provenance tagging
/// (BR-1, enforced by TASK-007).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The daemon's own instructions.
    System,
    /// End-user prompt text.
    User,
    /// Model-generated text (assistant turn).
    Model,
    /// A tool result. `path` is set when the result came from a specific file, so
    /// egress can match it against a privacy boundary.
    Tool {
        /// Tool that produced the result.
        tool: String,
        /// Repo-relative path the result concerns, when applicable.
        path: Option<String>,
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

    /// Append a tool result, tagged with the tool and (optionally) the file it
    /// concerns.
    pub fn push_tool_result(
        &mut self,
        tool: impl Into<String>,
        path: Option<String>,
        text: impl Into<String>,
    ) {
        let tool = tool.into();
        self.blocks.push(ContextBlock {
            role: BlockRole::Tool,
            text: text.into(),
            provenance: Provenance::Tool { tool, path },
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
}

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
                path: Some("a.rs".to_owned())
            }
        );
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
