//! The agent harness: the tool-use loop that reads, edits, and verifies code.
//!
//! This is the agentic core of Teton Code. It lands **local-first** (architecture
//! D-3): the loop drives the local [`Engine`](teton_inference::Engine) tier and
//! nothing else, so a freeform session can read → edit → verify a file with zero
//! egress (the offline AC-1 path, `tests/offline_session.rs`). Remote routing and
//! the single egress choke point that enforces privacy boundaries (BR-1) arrive
//! in TASK-010/TASK-007 and attach at the [`context::ProvenanceHook`] seam.
//!
//! The whole harness is shaped for **weak models** (BR-6, the product thesis):
//! short loops, a small tool set, and mandatory post-edit verification are the
//! default ([`turn_loop::HarnessConfig`]), not a degraded fallback. A strong
//! model runs the same loop with a longer leash.
//!
//! ## Module map
//! - [`tools`] — the built-in read/edit/glob/grep/shell tools, each jailed to the
//!   repo root; `edit` is exact-match and refuses ambiguous replacements.
//! - [`permissions`] — per-tool allow/ask/deny policy, the `permission_request`
//!   client round-trip over TASK-004's bus, and session-scoped grants.
//! - [`context`] — small-model context management: truncation, tool-result
//!   summarization, and the provenance-tagging seam for egress.
//! - [`duty`] — the shared duty seam (REQ-561): one [`duty::DutyRoute`], one
//!   [`duty::Duty`] trait, one local impl, one remote impl behind the egress
//!   choke point, and one output ceiling — for every model call the harness
//!   makes on its own behalf rather than on the user's.
//! - [`digest`] — what is `digest`-specific about the summarization duty
//!   (REQ-558): its [`digest::DIGEST_DUTY`] descriptor and the tool-result
//!   provenance bridge. Everything else it used to own now lives in [`duty`].
//! - [`triage`] — what is `triage`-specific about the grep-ranking duty
//!   (REQ-561): its [`triage::TRIAGE_DUTY`] descriptor, its output contract, and
//!   its prompt builder. The call site is [`tools::GrepTool`]'s
//!   [`Tool::refine`](tools::Tool::refine).
//! - [`completion`] — the [`completion::CompletionSource`] the loop drives: a
//!   local-[`Engine`](teton_inference::Engine) impl and a remote-[`Provider`](teton_providers::Provider)
//!   impl that streams through the egress choke point (BR-1/BR-2). This is what
//!   lets one loop run either tier.
//! - [`render`] — prompt rendering for the local tier (REQ-554): the model's
//!   native ChatML template when the loaded GGUF carries one, the flat
//!   `User:`/`Assistant:` transcript as the visible fallback.
//! - [`reply`] — model-reply scanning (BUG-147): the turn-boundary scanner that
//!   stops a weak model at its first tool call instead of letting it fabricate
//!   the rest of the transcript, the parse that reports dropped extra calls,
//!   and the display gate that keeps raw tool-call JSON out of the stream.
//! - [`turn_loop`] — the loop itself (named `turn_loop` because `loop` is a
//!   keyword): context assembly, model call, tool dispatch, result folding, and
//!   bounded termination.

pub mod completion;
pub mod context;
pub mod digest;
pub mod duty;
pub mod permissions;
pub(crate) mod render;
pub(crate) mod reply;
pub mod tools;
pub mod triage;
pub mod turn_loop;

pub use completion::{
    context_provenance, CompletionSource, LocalEngineSource, RemoteProviderSource, SourceTurn,
    TurnDecision,
};
pub use context::{
    ContextBlock, ContextManager, NoopProvenanceHook, Provenance, ProvenanceHook,
    RecordingProvenanceHook, ToolProvenance,
};
pub use digest::DIGEST_DUTY;
pub use duty::{Duty, DutyKind, DutyRoute};
pub use permissions::{
    PendingPermissions, PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
};
pub use tools::{RefinedOutcome, Tool, ToolContext, ToolDuties, ToolOutcome, ToolRegistry};
pub use triage::TRIAGE_DUTY;
pub use turn_loop::{
    build_system_prompt, run_session_turn, run_session_turn_with_source, HarnessConfig,
    HarnessError, SessionEvents, TurnOutcome,
};
