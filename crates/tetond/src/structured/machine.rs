//! The structured-mode phase state machine (D-4) with artifact gates.
//!
//! A structured session walks the ADLC flow **spec → architect → implement →
//! review**, and each transition is guarded by a gate: the artifact(s) the current
//! phase produces must exist and be minimally valid before the session may move
//! on. That is what makes cheap-model routing sound — the implement phase runs
//! against a task artifact an architect turn actually authored, not thin air.
//!
//! Two rules shape the design:
//!
//! - **Freeform is the degenerate case (BR-3).** A [`PhaseMachine::freeform`]
//!   session sits in [`Phase::Freeform`] with no gates and no transitions: it can
//!   never be *required* to produce an artifact. Entering structured mode is an
//!   explicit choice ([`PhaseMachine::structured`]).
//! - **A gate never generates silently.** A missing or still-templated artifact
//!   blocks the transition with an actionable [`GateError`] naming the file and
//!   what to do; the machine does not fabricate the artifact to get past its own
//!   gate.

use std::fmt;

use teton_protocol::events::{PhaseTransition, TaskArtifactRef};
use teton_protocol::Phase;

use super::artifacts::{is_authored, ArtifactKind, ArtifactStore, TaskArtifact};

/// Whether a session runs the full ADLC gates or a single freeform phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default (BR-3): one phase, no gates, no required artifacts.
    Freeform,
    /// Opt-in ADLC: gated spec → architect → implement → review.
    Structured,
}

/// A gate refusal or a request to advance past the end of the flow. Every variant
/// renders an actionable, content-free message (paths are config/repo-relative,
/// never artifact contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The artifact required to leave the current phase does not exist.
    Missing {
        /// Phase whose gate refused.
        phase: Phase,
        /// The missing artifact kind.
        kind: ArtifactKind,
        /// Repo-relative path the artifact should live at.
        path: String,
    },
    /// The artifact exists but is empty or a still-templated stub.
    Invalid {
        /// Phase whose gate refused.
        phase: Phase,
        /// The invalid artifact kind.
        kind: ArtifactKind,
        /// Repo-relative path of the offending artifact.
        path: String,
    },
    /// There is no phase to advance to — [`Phase::Review`] is terminal and
    /// [`Phase::Freeform`] has no structured flow.
    NoNextPhase {
        /// The current (final or freeform) phase.
        phase: Phase,
    },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::Missing { phase, kind, path } => write!(
                f,
                "cannot leave the {phase:?} phase: the {kind:?} artifact is missing. \
                 Author it at `{path}` before advancing."
            ),
            GateError::Invalid { phase, kind, path } => write!(
                f,
                "cannot leave the {phase:?} phase: the {kind:?} artifact at `{path}` is \
                 empty or an unfilled template. Fill in its placeholders before advancing."
            ),
            GateError::NoNextPhase { phase } => {
                write!(f, "the {phase:?} phase has no next phase to advance to")
            }
        }
    }
}

impl std::error::Error for GateError {}

/// A structured (or freeform) session's position in the ADLC flow.
#[derive(Debug, Clone)]
pub struct PhaseMachine {
    mode: Mode,
    phase: Phase,
    req_id: String,
}

impl PhaseMachine {
    /// A structured session for `req_id`, starting in the spec phase.
    #[must_use]
    pub fn structured(req_id: impl Into<String>) -> Self {
        Self {
            mode: Mode::Structured,
            phase: Phase::Spec,
            req_id: req_id.into(),
        }
    }

    /// A freeform session: one phase, no gates, no required artifacts (BR-3).
    #[must_use]
    pub fn freeform() -> Self {
        Self {
            mode: Mode::Freeform,
            phase: Phase::Freeform,
            req_id: String::new(),
        }
    }

    /// The session mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The current phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The owning requirement id (empty in freeform mode).
    #[must_use]
    pub fn req_id(&self) -> &str {
        &self.req_id
    }

    /// The next phase in the structured flow, or `None` if terminal or freeform.
    #[must_use]
    pub fn next_phase(&self) -> Option<Phase> {
        match self.mode {
            Mode::Freeform => None,
            Mode::Structured => match self.phase {
                Phase::Spec => Some(Phase::Architect),
                Phase::Architect => Some(Phase::Implement),
                Phase::Implement => Some(Phase::Review),
                _ => None,
            },
        }
    }

    /// Whether the session has reached the end of its flow (freeform is always
    /// terminal; structured is terminal at [`Phase::Review`]).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.next_phase().is_none()
    }

    /// The artifact kinds a phase produces (and whose gate guards leaving it).
    #[must_use]
    fn gate_kinds(phase: Phase) -> &'static [ArtifactKind] {
        match phase {
            Phase::Spec => &[ArtifactKind::Requirement],
            Phase::Architect => &[ArtifactKind::Plan, ArtifactKind::Task],
            Phase::Implement => &[ArtifactKind::Task],
            _ => &[],
        }
    }

    /// The artifacts to inject into model context for the current phase.
    ///
    /// The implement phase carries the **task artifact** — the mechanism that
    /// makes a cheap model viable on the token-heavy implementation work (the
    /// REQ-544 thesis). Other phases carry their inputs. A missing artifact is
    /// simply absent (never fabricated).
    #[must_use]
    pub fn context_artifacts(&self, store: &ArtifactStore) -> Vec<TaskArtifact> {
        let kinds: &[ArtifactKind] = match self.phase {
            Phase::Architect => &[ArtifactKind::Requirement],
            Phase::Implement => &[ArtifactKind::Task],
            Phase::Review => &[ArtifactKind::Requirement, ArtifactKind::Task],
            _ => &[],
        };
        kinds
            .iter()
            .filter_map(|kind| store.load(&self.req_id, *kind))
            .collect()
    }

    /// Attempt to advance to the next phase, checking the gate for the artifact(s)
    /// the current phase produces.
    ///
    /// On success the machine moves to the next phase and returns the
    /// [`PhaseTransition`] (with the artifact refs carried across the gate) to
    /// broadcast. On a gate failure the machine does **not** move and returns a
    /// [`GateError`] with an actionable message.
    ///
    /// # Errors
    /// [`GateError::Missing`] / [`GateError::Invalid`] when the gate's artifact is
    /// absent or an unfilled stub; [`GateError::NoNextPhase`] at the end of the
    /// flow or in freeform mode.
    pub fn try_advance(&mut self, store: &ArtifactStore) -> Result<PhaseTransition, GateError> {
        let Some(to) = self.next_phase() else {
            return Err(GateError::NoNextPhase { phase: self.phase });
        };

        let mut refs: Vec<TaskArtifactRef> = Vec::new();
        for &kind in Self::gate_kinds(self.phase) {
            let artifact = self.require_valid(store, kind)?;
            refs.push(artifact.to_ref());
        }

        let from = self.phase;
        self.phase = to;
        Ok(PhaseTransition {
            from_phase: Some(from),
            to_phase: to,
            artifacts: refs,
        })
    }

    /// Load an artifact and require it to be authored, or fail the gate.
    fn require_valid(
        &self,
        store: &ArtifactStore,
        kind: ArtifactKind,
    ) -> Result<TaskArtifact, GateError> {
        let path = store.rel_path(&self.req_id, kind);
        match store.load(&self.req_id, kind) {
            None => Err(GateError::Missing {
                phase: self.phase,
                kind,
                path,
            }),
            Some(artifact) if !is_authored(&artifact.content) => Err(GateError::Invalid {
                phase: self.phase,
                kind,
                path,
            }),
            Some(artifact) => Ok(artifact),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_repo() -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-machine-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Author every artifact so the gates pass end to end.
    fn author_all(store: &ArtifactStore, req: &str) {
        store
            .write(
                req,
                ArtifactKind::Requirement,
                "# R\n\nreal requirement text",
            )
            .unwrap();
        store
            .write(req, ArtifactKind::Plan, "# P\n\nreal plan text")
            .unwrap();
        store
            .write(req, ArtifactKind::Task, "# T\n\nreal task text")
            .unwrap();
    }

    #[test]
    fn freeform_never_transitions_and_needs_no_artifacts() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        let mut machine = PhaseMachine::freeform();
        assert_eq!(machine.mode(), Mode::Freeform);
        assert_eq!(machine.phase(), Phase::Freeform);
        assert!(machine.is_terminal());
        assert!(machine.context_artifacts(&store).is_empty());
        // BR-3: advancing a freeform session is a no-op refusal, not a gate on a
        // missing artifact.
        match machine.try_advance(&store) {
            Err(GateError::NoNextPhase { phase }) => assert_eq!(phase, Phase::Freeform),
            other => panic!("freeform must not gate on artifacts: {other:?}"),
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn structured_flow_advances_through_all_four_phases_when_gates_pass() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        author_all(&store, "demo");

        let mut machine = PhaseMachine::structured("demo");
        assert_eq!(machine.phase(), Phase::Spec);

        let t1 = machine.try_advance(&store).expect("spec → architect");
        assert_eq!(t1.from_phase, Some(Phase::Spec));
        assert_eq!(t1.to_phase, Phase::Architect);
        assert_eq!(t1.artifacts.len(), 1);
        assert_eq!(t1.artifacts[0].path, ".teton/demo/requirement.md");

        let t2 = machine.try_advance(&store).expect("architect → implement");
        assert_eq!(t2.to_phase, Phase::Implement);
        // The architect gate carries both the plan and the task forward.
        assert_eq!(t2.artifacts.len(), 2);

        let t3 = machine.try_advance(&store).expect("implement → review");
        assert_eq!(t3.to_phase, Phase::Review);

        // Review is terminal.
        assert!(machine.is_terminal());
        assert!(matches!(
            machine.try_advance(&store),
            Err(GateError::NoNextPhase { .. })
        ));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_missing_requirement_blocks_the_spec_gate_with_an_actionable_message() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        let mut machine = PhaseMachine::structured("demo");

        let err = machine.try_advance(&store).expect_err("no requirement yet");
        match &err {
            GateError::Missing { phase, path, .. } => {
                assert_eq!(*phase, Phase::Spec);
                assert_eq!(path, ".teton/demo/requirement.md");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains(".teton/demo/requirement.md"));
        assert!(msg.contains("Author it"), "message is actionable: {msg}");
        // The machine did not move.
        assert_eq!(machine.phase(), Phase::Spec);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_still_templated_stub_blocks_the_gate_and_is_not_auto_generated() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        // Scaffold leaves content placeholders — an unauthored stub.
        store.scaffold("demo", "A demo").expect("scaffold");

        let mut machine = PhaseMachine::structured("demo");
        let err = machine.try_advance(&store).expect_err("stub is invalid");
        match err {
            GateError::Invalid { kind, .. } => assert_eq!(kind, ArtifactKind::Requirement),
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(
            machine.phase(),
            Phase::Spec,
            "gate did not advance on a stub"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn implement_phase_carries_the_task_artifact_in_context() {
        let repo = temp_repo();
        let store = ArtifactStore::new(&repo);
        store
            .write(
                "demo",
                ArtifactKind::Task,
                "# Task\n\nDISTINCTIVE-TASK-MARKER",
            )
            .unwrap();

        let mut machine = PhaseMachine::structured("demo");
        machine.phase = Phase::Implement; // jump straight to implement for the check

        let artifacts = machine.context_artifacts(&store);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, ArtifactKind::Task);
        assert!(artifacts[0].content.contains("DISTINCTIVE-TASK-MARKER"));
        std::fs::remove_dir_all(&repo).ok();
    }
}
