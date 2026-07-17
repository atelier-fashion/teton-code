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
//! - [`context`] — small-model context management: truncation, local
//!   summarization, and the provenance-tagging seam for egress.
//! - [`turn_loop`] — the loop itself (named `turn_loop` because `loop` is a
//!   keyword): context assembly, model call, tool dispatch, result folding, and
//!   bounded termination.

pub mod context;
pub mod permissions;
pub mod tools;
pub mod turn_loop;

pub use context::{
    ContextBlock, ContextManager, NoopProvenanceHook, Provenance, ProvenanceHook,
    RecordingProvenanceHook,
};
pub use permissions::{
    PendingPermissions, PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
};
pub use tools::{Tool, ToolContext, ToolOutcome, ToolRegistry};
pub use turn_loop::{
    build_system_prompt, run_session_turn, HarnessConfig, HarnessError, SessionEvents, TurnOutcome,
};
