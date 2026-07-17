//! Structured (ADLC) mode: the phase state machine, artifact gates, and storage.
//!
//! This is the **generic extraction** of the ADLC (D-4), opt-in per BR-3: a phase
//! machine over the core [`Phase`](teton_protocol::Phase) enum
//! (spec → architect → implement → review) with artifact gates that carry
//! intelligence forward so a cheap model can execute the implement phase. It is
//! *not* the author's personal toolkit: no REQ counters, no global state, no gate
//! scripts — phases, artifacts, and gates only. Freeform is the degenerate
//! single-phase case through the same types (BR-3).
//!
//! ## Module map
//! - [`artifacts`] — `TaskArtifact` storage under `<repo>/.teton/`, plus the
//!   bundled-template scaffold for a fresh repo (OQ-5).
//! - [`templates`] — the generic requirement / plan / task skeletons, compiled in.
//! - [`machine`] — the [`PhaseMachine`]: gated transitions producing a
//!   `phase_transition`, and the per-phase context artifacts (the implement phase
//!   carries the task artifact).

pub mod artifacts;
pub mod machine;
pub mod templates;

pub use artifacts::{is_authored, ArtifactKind, ArtifactStore, TaskArtifact};
pub use machine::{GateError, Mode, PhaseMachine};
