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
//! Both duties are enforced in **two currencies**: whitespace-approximated
//! tokens ([`approx_tokens`]) and UTF-8 bytes. The token heuristic undercounts
//! pathological content — a minified single-line file is a handful of "words"
//! but tens of thousands of real BPE tokens — so every budget here carries a
//! byte-denominated twin sized to the local engine's window (bytes are a
//! conservative proxy for BPE tokens: code averages ≳2 bytes per BPE token).
//! This is what keeps one dense block from pushing an assembled prompt past the
//! engine window and killing the turn.
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

/// Manages the assembled context for one session under a token budget and a
/// byte budget (the engine-window currency — see the module docs).
#[derive(Debug, Clone)]
pub struct ContextManager {
    system: String,
    blocks: Vec<ContextBlock>,
    budget_tokens: usize,
    budget_bytes: usize,
    truncated: bool,
}

impl ContextManager {
    /// A manager with the given system prompt and token budget. The byte budget
    /// defaults to `budget_tokens` × [`APPROX_BYTES_PER_TOKEN`] — the same
    /// relationship `HarnessConfig::default` encodes; override it with
    /// [`ContextManager::with_budget_bytes`] to match a specific engine window.
    #[must_use]
    pub fn new(system: impl Into<String>, budget_tokens: usize) -> Self {
        Self {
            system: system.into(),
            blocks: Vec::new(),
            budget_tokens,
            budget_bytes: budget_tokens.saturating_mul(APPROX_BYTES_PER_TOKEN),
            truncated: false,
        }
    }

    /// Set the byte budget for the assembled context (engine-window currency).
    #[must_use]
    pub fn with_budget_bytes(mut self, budget_bytes: usize) -> Self {
        self.budget_bytes = budget_bytes;
        self
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

    /// Estimated total bytes (system + all blocks) — the engine-window currency
    /// that catches what the whitespace heuristic undercounts.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.system.len() + self.blocks.iter().map(|b| b.text.len()).sum::<usize>()
    }

    /// Drop the oldest blocks until the estimate fits **both** budgets (tokens
    /// and bytes). The system prompt and the single most recent block are always
    /// preserved — but if that last block alone still busts the byte budget (a
    /// pathological fold, a giant paste), its text is clamped in place with an
    /// elision marker. The assembled prompt is therefore bounded in bytes no
    /// matter what any single block carries: the turn degrades instead of
    /// handing the engine an over-window prompt it can only refuse.
    pub fn truncate_to_budget(&mut self) {
        while (self.estimated_tokens() > self.budget_tokens
            || self.estimated_bytes() > self.budget_bytes)
            && self.blocks.len() > 1
        {
            self.blocks.remove(0);
            self.truncated = true;
        }
        if self.estimated_bytes() > self.budget_bytes {
            // Floor keeps a degenerate configuration (system prompt near or over
            // the whole byte budget) from clamping the block to nothing.
            let room = self
                .budget_bytes
                .saturating_sub(self.system.len())
                .max(1_024);
            if let Some(last) = self.blocks.last_mut() {
                if last.text.len() > room {
                    last.text = truncate_middle(&last.text, room);
                }
            }
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

/// Bytes-per-whitespace-token bridge between the two budget currencies.
///
/// A whitespace "token" of source code averages ~7–8 bytes (word plus
/// separator), so a token budget of N is consistent with a byte budget of
/// N × 8. At the local engine's window this is also the safe direction: 8 bytes
/// per whitespace word ≈ 2 bytes per real BPE token for code, comfortably above
/// the ~2-bytes-per-token floor valid UTF-8 tokenizes at in practice.
pub const APPROX_BYTES_PER_TOKEN: usize = 8;

/// Byte ceiling on the tool-result text handed to the summarizer engine.
///
/// The summarizer's own prompt must fit the engine window too — sending an
/// unbounded result to the engine that exists to shrink it just moves the
/// over-window failure one call earlier (the pre-fix behavior). 16 KiB is at
/// most ~8k BPE tokens of pathological input, about half the 16,384-token
/// window (`LOCAL_ENGINE_N_CTX`), leaving ample room for the instruction and
/// generation.
pub const SUMMARIZER_INPUT_MAX_BYTES: usize = 16_384;

/// Truncate `text` to at most `max_bytes`, keeping the head and tail with an
/// elision marker between them (errors cluster at the end of build logs, paths
/// and signatures at the top of files). Splits on `char` boundaries; returns
/// the text unchanged when it already fits.
#[must_use]
pub fn truncate_middle(text: &str, max_bytes: usize) -> String {
    const MARKER: &str =
        "\n[... middle elided: content truncated to fit the local context window ...]\n";
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let keep = max_bytes.saturating_sub(MARKER.len());
    if keep < 64 {
        // Degenerate cap: no room for a useful head/tail split.
        return text[..floor_char_boundary(text, max_bytes)].to_owned();
    }
    let head_len = keep * 2 / 3;
    let head_end = floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len() - (keep - head_len));
    format!("{}{MARKER}{}", &text[..head_end], &text[tail_start..])
}

/// Largest index ≤ `i` that is a `char` boundary of `s`.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index ≥ `i` that is a `char` boundary of `s`.
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// What [`summarize_if_large`] did with a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeOutcome {
    /// The text to fold into context: the engine's summary, a mechanical
    /// truncation (engine failure), or the original (under threshold).
    pub text: String,
    /// The engine error hit while summarizing, when the summary fell back to
    /// mechanical truncation. The caller MUST surface this (log or event) — the
    /// summarization duty guards the context window, so its failure is never
    /// allowed to be silent.
    pub engine_error: Option<String>,
}

/// Summarize a tool result with the local engine when it is larger than
/// `threshold_tokens` (whitespace tokens) **or** its byte-denominated twin
/// (`threshold_tokens` × [`APPROX_BYTES_PER_TOKEN`]); otherwise return it
/// unchanged. The byte trigger is what catches whitespace-poor content — a
/// minified single-line file is a handful of "words" but tens of thousands of
/// BPE tokens, exactly the input the whitespace heuristic waves through.
///
/// This keeps a large file read or a noisy log from evicting the conversation on
/// a small model. Summarization is a *local* duty (BR-8 latency, not
/// intelligence): the engine passed here is the local tier. The text sent to the
/// engine is bounded by [`SUMMARIZER_INPUT_MAX_BYTES`] so the summarizer prompt
/// itself always fits the engine window. On an engine error the result is
/// truncated **mechanically** to the same threshold — never folded raw, which
/// would silently no-op the duty — and the error is reported on the outcome for
/// the caller to surface.
#[must_use]
pub fn summarize_if_large(
    engine: &Mutex<dyn Engine>,
    tool: &str,
    text: &str,
    threshold_tokens: usize,
) -> SummarizeOutcome {
    let threshold_bytes = threshold_tokens.saturating_mul(APPROX_BYTES_PER_TOKEN);
    if approx_tokens(text) <= threshold_tokens && text.len() <= threshold_bytes {
        return SummarizeOutcome {
            text: text.to_owned(),
            engine_error: None,
        };
    }
    let bounded = truncate_middle(text, SUMMARIZER_INPUT_MAX_BYTES);
    let prompt = format!(
        "Summarize the following `{tool}` tool output in a few lines, preserving \
         file paths, symbol names, and any errors. Output only the summary.\n\n{bounded}"
    );
    let params = GenParams::default();
    let guard = engine.lock().expect("engine mutex poisoned");
    match guard.complete(&prompt, &params, &mut |_| {}) {
        Ok(completion) => SummarizeOutcome {
            text: format!(
                "[summarized {tool} output — {} tokens elided]\n{}",
                approx_tokens(text),
                completion.text
            ),
            engine_error: None,
        },
        Err(err) => SummarizeOutcome {
            text: format!(
                "[oversized {tool} output truncated mechanically — the local \
                 summarizer was unavailable]\n{}",
                truncate_middle(text, threshold_bytes)
            ),
            engine_error: Some(err.to_string()),
        },
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
        assert_eq!(out.text, "short output");
        assert_eq!(out.engine_error, None);
    }

    #[test]
    fn large_tool_results_are_summarized_by_the_local_engine() {
        let engine = Mutex::new(MockEngine::with_response("mock-3b", "CONDENSED"));
        let big = "word ".repeat(500);
        let out = summarize_if_large(&engine, "grep", &big, 50);
        assert!(out.text.contains("summarized grep output"));
        assert!(out.text.contains("CONDENSED"));
        assert_eq!(out.engine_error, None);
    }

    #[test]
    fn whitespace_poor_but_byte_huge_results_trigger_summarization() {
        // The dogfooded failure mode: a minified single-line file is a handful of
        // whitespace "words" but enormous in bytes/BPE. The byte-denominated
        // trigger must summarize it even though the token trigger waves it through.
        let engine = Mutex::new(MockEngine::with_response("mock-3b", "CONDENSED"));
        let minified = "x".repeat(100_000); // 1 whitespace token, 100 KB
        assert!(approx_tokens(&minified) <= 100);
        let out = summarize_if_large(&engine, "read", &minified, 100);
        assert!(out.text.contains("summarized read output"));
        assert!(out.text.contains("CONDENSED"));
    }

    /// An engine that records the byte length of every prompt it is handed.
    struct PromptLenEngine {
        seen: std::sync::Arc<Mutex<Vec<usize>>>,
    }

    impl Engine for PromptLenEngine {
        fn model_id(&self) -> &str {
            "prompt-len"
        }
        fn complete(
            &self,
            prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str),
        ) -> Result<teton_inference::Completion, teton_inference::EngineError> {
            self.seen.lock().expect("seen poisoned").push(prompt.len());
            Ok(teton_inference::Completion {
                text: "SUMMARY".to_owned(),
                prompt_tokens: 1,
                completion_tokens: 1,
            })
        }
    }

    #[test]
    fn summarizer_input_is_bounded_in_engine_window_bytes() {
        // The summarizer prompt must fit the engine window regardless of how big
        // the tool result is — pre-fix, the ENTIRE result rode the prompt.
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let engine = Mutex::new(PromptLenEngine {
            seen: std::sync::Arc::clone(&seen),
        });
        let huge = "word ".repeat(200_000); // 1 MB
        let out = summarize_if_large(&engine, "shell", &huge, 100);
        assert!(out.text.contains("SUMMARY"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        // Bounded input plus the fixed instruction preamble; generous slack.
        assert!(
            seen[0] <= SUMMARIZER_INPUT_MAX_BYTES + 512,
            "summarizer prompt was {} bytes — unbounded input reached the engine",
            seen[0]
        );
    }

    #[test]
    fn engine_failure_falls_back_to_bounded_mechanical_truncation() {
        // Pre-fix: Err(_) => text.to_owned() folded the raw oversized result and
        // told nobody. Now the fallback is mechanically truncated to the same
        // threshold, and the error is reported for the caller to surface.
        let engine = Mutex::new(MockEngine::unavailable("mock", "unloaded under pressure"));
        let big = "word ".repeat(50_000); // 250 KB
        let threshold_tokens = 100;
        let out = summarize_if_large(&engine, "read", &big, threshold_tokens);
        assert!(out.text.contains("truncated mechanically"));
        assert!(
            out.text.len() <= threshold_tokens * APPROX_BYTES_PER_TOKEN + 256,
            "fallback fold was {} bytes — the raw result leaked through",
            out.text.len()
        );
        let err = out.engine_error.expect("engine failure must be reported");
        assert!(err.contains("unloaded under pressure"));
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail_within_the_cap() {
        let text = format!("{}{}{}", "HEAD ".repeat(100), "x".repeat(10_000), " TAIL");
        let out = truncate_middle(&text, 1_000);
        assert!(out.len() <= 1_000);
        assert!(out.starts_with("HEAD "));
        assert!(out.ends_with(" TAIL"));
        assert!(out.contains("middle elided"));
        // Under the cap: untouched.
        assert_eq!(truncate_middle("small", 1_000), "small");
    }

    #[test]
    fn truncate_middle_respects_char_boundaries() {
        // Multi-byte chars at the cut points must not panic or split.
        let text = "é".repeat(2_000); // 4,000 bytes of 2-byte chars
        let out = truncate_middle(&text, 500);
        assert!(out.len() <= 500);
        assert!(out.contains("middle elided"));
    }

    #[test]
    fn truncation_evicts_on_bytes_even_when_tokens_fit() {
        // Two dense single-word blocks: 2 tokens (far under the token budget) but
        // way over the byte budget — the byte currency must drive eviction.
        let mut ctx = ContextManager::new("sys", 10_000).with_budget_bytes(5_000);
        ctx.push_user("a".repeat(4_000));
        ctx.push_user("b".repeat(4_000));
        assert!(ctx.estimated_tokens() < 10_000);
        ctx.truncate_to_budget();
        assert!(ctx.was_truncated());
        assert_eq!(ctx.blocks().len(), 1);
        assert!(ctx.estimated_bytes() <= 5_000);
    }

    #[test]
    fn a_single_oversized_block_is_clamped_in_place() {
        // Eviction preserves the most recent block, so a lone pathological block
        // must be clamped rather than handed to the engine over-window.
        let mut ctx = ContextManager::new("sys", 10_000).with_budget_bytes(5_000);
        ctx.push_user("z".repeat(50_000));
        ctx.truncate_to_budget();
        assert_eq!(ctx.blocks().len(), 1);
        assert!(
            ctx.estimated_bytes() <= 5_000,
            "assembled context is {} bytes — the clamp did not bound it",
            ctx.estimated_bytes()
        );
        assert!(ctx.blocks()[0].text.contains("middle elided"));
    }
}
