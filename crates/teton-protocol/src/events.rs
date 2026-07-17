//! Daemon→client events.
//!
//! The daemon broadcasts events to subscribed clients (ADR-002's
//! event-subscription model). Every event travels inside an [`EventEnvelope`]:
//! shared metadata (optional session scope, a broadcast sequence number) plus a
//! tagged [`Event`] discriminated by a snake_case `event` name.
//!
//! The `event` names are the contract fixed by REQ-544's System Model → Events
//! table: `route_decided`, `privacy_block`, `phase_transition`, `cost_recorded`,
//! `provider_degraded`, `daemon_client_attach`. Three further events —
//! `session_update`, `permission_request`, `model_lifecycle` — carry the
//! streaming turn, permission prompts, and local-model lifecycle (BR-9); the
//! first two borrow ACP vocabulary.

use serde::{Deserialize, Serialize};

use crate::{ClientKind, Phase, ProtocolVersion, ProviderId, RequestId, SessionId};

/// A broadcast event plus its shared envelope metadata.
///
/// The [`Event`] is internally tagged and flattened, so the wire form is a flat
/// object: `{ "session_id": …, "seq": …, "event": "route_decided", … }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Session this event belongs to, or `None` for daemon-scoped events
    /// (`daemon_client_attach`, `model_lifecycle`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<SessionId>,
    /// Monotonic per-stream sequence number for ordering and gap detection.
    pub seq: u64,
    /// The event itself.
    #[serde(flatten)]
    pub event: Event,
}

impl EventEnvelope {
    /// Wraps `event` with a sequence number and optional session scope.
    pub fn new(seq: u64, session_id: Option<SessionId>, event: Event) -> Self {
        Self {
            session_id,
            seq,
            event,
        }
    }

    /// The wire `event` name of the wrapped event (matches the spec table).
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        self.event.name()
    }
}

/// One broadcast event, discriminated on the wire by a snake_case `event` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Streaming update within a prompt turn. ACP: `session/update`.
    SessionUpdate(SessionUpdate),
    /// A model was selected for a step (spec: `route_decided`). Teton
    /// differentiator — no ACP equivalent.
    RouteDecided(RouteDecided),
    /// Boundary content would have gone remote (spec: `privacy_block`). Teton
    /// differentiator — no ACP equivalent.
    PrivacyBlock(PrivacyBlock),
    /// A model call completed and produced a cost record (spec: `cost_recorded`).
    CostRecorded(CostRecorded),
    /// An adapter fell back to another provider (spec: `provider_degraded`).
    ProviderDegraded(ProviderDegraded),
    /// Local-model lifecycle progress: download / benchmark / step-down (BR-9).
    ModelLifecycle(ModelLifecycle),
    /// The harness needs a permission decision. ACP: `session/request_permission`.
    PermissionRequest(PermissionRequest),
    /// A structured-mode phase gate passed (spec: `phase_transition`).
    PhaseTransition(PhaseTransition),
    /// A client attached to the daemon (spec: `daemon_client_attach`).
    DaemonClientAttach(DaemonClientAttach),
}

impl Event {
    /// The wire `event` name, identical to the serialized tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Event::SessionUpdate(_) => "session_update",
            Event::RouteDecided(_) => "route_decided",
            Event::PrivacyBlock(_) => "privacy_block",
            Event::CostRecorded(_) => "cost_recorded",
            Event::ProviderDegraded(_) => "provider_degraded",
            Event::ModelLifecycle(_) => "model_lifecycle",
            Event::PermissionRequest(_) => "permission_request",
            Event::PhaseTransition(_) => "phase_transition",
            Event::DaemonClientAttach(_) => "daemon_client_attach",
        }
    }
}

// ---------------------------------------------------------------------------
// session_update (ACP: session/update)
// ---------------------------------------------------------------------------

/// A streaming update within a prompt turn. ACP: `session/update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUpdate {
    /// The specific update.
    pub update: SessionUpdatePayload,
}

/// The kinds of streaming update a turn can emit. ACP: `SessionUpdate` variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionUpdatePayload {
    /// A chunk of assistant text. ACP: `agent_message_chunk`.
    AgentMessageChunk {
        /// The text delta.
        text: String,
    },
    /// A tool call started. ACP: `tool_call`.
    ToolCall {
        /// Correlates the call with later updates.
        tool_call_id: String,
        /// Human-facing title.
        title: String,
        /// Current status.
        status: ToolCallStatus,
    },
    /// A tool call changed status. ACP: `tool_call_update`.
    ToolCallUpdate {
        /// The call being updated.
        tool_call_id: String,
        /// New status.
        status: ToolCallStatus,
    },
    /// A proposed file change. ACP: the `diff` content shape.
    Diff {
        /// Repo-relative path.
        path: String,
        /// Prior contents, or `None` for a new file.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        old_text: Option<String>,
        /// Proposed contents.
        new_text: String,
    },
    /// The agent's current plan. ACP: `plan`.
    Plan {
        /// Ordered plan entries.
        entries: Vec<PlanEntry>,
    },
}

/// Status of a tool call. ACP: `ToolCallStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Awaiting permission or scheduling.
    Pending,
    /// Executing.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// One entry in an agent plan. ACP: a `PlanEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// What the step will do.
    pub content: String,
    /// Step status.
    pub status: PlanEntryStatus,
}

/// Status of a plan entry. ACP: `PlanEntryStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    /// Not started.
    Pending,
    /// Underway.
    InProgress,
    /// Done.
    Completed,
}

// ---------------------------------------------------------------------------
// route_decided (Teton differentiator)
// ---------------------------------------------------------------------------

/// The router picked a provider for a step (spec Events: `route_decided`).
///
/// Every routing decision emits this with its `reason` — the legibility promise
/// (BR-5). The `session` scoping lives in the [`EventEnvelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecided {
    /// Phase that drove the decision; `None` for heuristic freeform routing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
    /// Provider selected.
    pub provider_id: ProviderId,
    /// Concrete model chosen, when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// The policy rule (or heuristic) that fired, as a user-facing sentence.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// privacy_block (Teton differentiator)
// ---------------------------------------------------------------------------

/// Boundary content would have entered a remote call (spec: `privacy_block`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyBlock {
    /// Repo-relative path of the boundary-protected content.
    pub path: String,
    /// Provider the content would have reached.
    pub provider_id: ProviderId,
    /// What the daemon did instead.
    pub action: PrivacyAction,
}

/// The action the egress choke point took on a would-be boundary violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyAction {
    /// The offending content was removed from the outbound payload.
    Stripped,
    /// The whole call was re-routed to the local tier.
    ReroutedToLocal,
}

// ---------------------------------------------------------------------------
// cost_recorded
// ---------------------------------------------------------------------------

/// A completed model call's cost record (spec entity `CostRecord`).
///
/// One record per model call; the cost meter is derived only from these (BR-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRecord {
    /// Session that incurred the cost.
    pub session_id: SessionId,
    /// Phase, or `None` in freeform mode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
    /// Provider billed.
    pub provider_id: ProviderId,
    /// Concrete model billed.
    pub model: String,
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cost in integer micro-dollars (1e-6 USD). Spec entity field `usd`, sent
    /// as an integer so money never rounds on the wire.
    pub usd_micros: i64,
}

/// Event payload wrapping a [`CostRecord`] (spec Events: `cost_recorded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRecorded {
    /// The record for the completed call.
    pub record: CostRecord,
}

// ---------------------------------------------------------------------------
// provider_degraded
// ---------------------------------------------------------------------------

/// An adapter fell back to another provider (spec: `provider_degraded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDegraded {
    /// Provider that failed.
    pub provider_id: ProviderId,
    /// Why it failed.
    pub failure_class: FailureClass,
    /// Provider used instead, if a fallback existed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
}

/// Classification of a provider failure that triggered degradation (BR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The provider produced malformed or unusable tool calls.
    ToolCallFailure,
    /// The call timed out.
    Timeout,
    /// The provider rate-limited the call.
    RateLimited,
    /// The connection failed.
    ConnectionError,
    /// The response could not be parsed.
    InvalidResponse,
}

// ---------------------------------------------------------------------------
// model_lifecycle (BR-9 — Teton differentiator, no ACP equivalent)
// ---------------------------------------------------------------------------

/// Local-model lifecycle progress (BR-9): probe → download → benchmark →
/// runtime pressure adaptation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelLifecycle {
    /// The model this update concerns (e.g. a GGUF identifier).
    pub model_id: String,
    /// The lifecycle stage reached.
    pub stage: ModelLifecycleStage,
}

/// A stage in the local-model lifecycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ModelLifecycleStage {
    /// First-run hardware probe result (RAM/disk/GPU class → candidate tier).
    Probed {
        /// Detected system RAM in bytes.
        ram_bytes: u64,
        /// Whether the machine cleared the hardware floor for a local tier.
        above_floor: bool,
    },
    /// Download progress for the selected model.
    Download {
        /// Bytes fetched so far.
        downloaded_bytes: u64,
        /// Total bytes, when the length is known.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        total_bytes: Option<u64>,
    },
    /// Post-download micro-benchmark result (validates the BR-8 latency duty).
    Benchmark {
        /// Measured time to first token, in milliseconds.
        first_token_ms: u32,
        /// Measured decode throughput in tokens/second.
        tokens_per_sec: f32,
    },
    /// The model is loaded and serving.
    Ready,
    /// The tier auto-stepped down after a failed duty (benchmark or pressure).
    SteppedDown {
        /// Model stepped away from.
        from_model: String,
        /// Model stepped down to.
        to_model: String,
        /// User-facing reason.
        reason: String,
    },
    /// The local tier is cleanly absent (below floor, or under memory pressure).
    Disabled {
        /// User-facing reason.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// permission_request (ACP: session/request_permission)
// ---------------------------------------------------------------------------

/// The harness needs a permission decision (spec: `permission_request`).
///
/// The client replies with [`crate::methods::PermissionRespondParams`], keyed by
/// `request_id`. ACP: `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Correlates with the client's later response.
    pub request_id: RequestId,
    /// Tool the harness wants to run.
    pub tool_name: String,
    /// Human-facing description of the pending action.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// The choices offered to the user.
    pub options: Vec<PermissionOption>,
}

/// One offered permission choice. ACP: `PermissionOption`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// Stable id echoed back in the response's `option_id`.
    pub option_id: String,
    /// User-facing label.
    pub label: String,
    /// Semantic kind (drives default styling/shortcuts).
    pub kind: PermissionOptionKind,
}

/// Semantic kind of a permission option. ACP: `PermissionOptionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this one time.
    AllowOnce,
    /// Allow and remember for the session.
    AllowAlways,
    /// Reject this one time.
    RejectOnce,
    /// Reject and remember for the session.
    RejectAlways,
}

// ---------------------------------------------------------------------------
// phase_transition
// ---------------------------------------------------------------------------

/// A structured-mode phase gate passed (spec: `phase_transition`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTransition {
    /// Phase left, or `None` when entering the first phase.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_phase: Option<Phase>,
    /// Phase entered.
    pub to_phase: Phase,
    /// ADLC artifacts carried across the gate.
    pub artifacts: Vec<TaskArtifactRef>,
}

/// A reference to an ADLC artifact (spec entity `TaskArtifact`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactRef {
    /// Owning requirement id.
    pub req_id: String,
    /// Phase that produced the artifact.
    pub phase: Phase,
    /// Repo-relative path to the artifact.
    pub path: String,
}

// ---------------------------------------------------------------------------
// daemon_client_attach
// ---------------------------------------------------------------------------

/// A client attached to the daemon (spec: `daemon_client_attach`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonClientAttach {
    /// The kind of client that attached.
    pub client_kind: ClientKind,
    /// Protocol version negotiated with that client.
    pub protocol_version: ProtocolVersion,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
    }

    /// Wraps an event, round-trips the envelope, and returns the wire object so
    /// callers can assert on the `event` tag.
    fn envelope_wire(event: Event) -> serde_json::Value {
        let env = EventEnvelope::new(1, Some(SessionId::from("s1")), event);
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn envelope_is_flat_and_tagged_by_event_name() {
        let wire = envelope_wire(Event::RouteDecided(RouteDecided {
            phase: Some(Phase::Architect),
            provider_id: ProviderId::from("anthropic"),
            model: Some("opus".to_owned()),
            reason: "architecture phase routes to the frontier tier".to_owned(),
        }));
        // Flattened: envelope metadata and the payload share one object.
        assert_eq!(wire["event"], "route_decided");
        assert_eq!(wire["seq"], 1);
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["provider_id"], "anthropic");
    }

    #[test]
    fn event_names_match_the_spec_events_table() {
        // The six names fixed by REQ-544's Events table, plus the three
        // streaming/lifecycle events. `name()` must equal the serialized tag.
        let cases: Vec<(Event, &str)> = vec![
            (
                Event::RouteDecided(RouteDecided {
                    phase: None,
                    provider_id: ProviderId::from("p"),
                    model: None,
                    reason: "r".to_owned(),
                }),
                "route_decided",
            ),
            (
                Event::PrivacyBlock(PrivacyBlock {
                    path: "secret.txt".to_owned(),
                    provider_id: ProviderId::from("p"),
                    action: PrivacyAction::Stripped,
                }),
                "privacy_block",
            ),
            (
                Event::PhaseTransition(PhaseTransition {
                    from_phase: Some(Phase::Spec),
                    to_phase: Phase::Architect,
                    artifacts: vec![],
                }),
                "phase_transition",
            ),
            (
                Event::CostRecorded(CostRecorded {
                    record: CostRecord {
                        session_id: SessionId::from("s"),
                        phase: None,
                        provider_id: ProviderId::from("p"),
                        model: "m".to_owned(),
                        input_tokens: 1,
                        output_tokens: 2,
                        usd_micros: 1234,
                    },
                }),
                "cost_recorded",
            ),
            (
                Event::ProviderDegraded(ProviderDegraded {
                    provider_id: ProviderId::from("p"),
                    failure_class: FailureClass::Timeout,
                    fallback_id: Some(ProviderId::from("q")),
                }),
                "provider_degraded",
            ),
            (
                Event::DaemonClientAttach(DaemonClientAttach {
                    client_kind: ClientKind::Cli,
                    protocol_version: crate::PROTOCOL_VERSION,
                }),
                "daemon_client_attach",
            ),
            (
                Event::SessionUpdate(SessionUpdate {
                    update: SessionUpdatePayload::AgentMessageChunk {
                        text: "hi".to_owned(),
                    },
                }),
                "session_update",
            ),
            (
                Event::PermissionRequest(PermissionRequest {
                    request_id: RequestId::from("r"),
                    tool_name: "shell".to_owned(),
                    description: None,
                    options: vec![],
                }),
                "permission_request",
            ),
            (
                Event::ModelLifecycle(ModelLifecycle {
                    model_id: "qwen".to_owned(),
                    stage: ModelLifecycleStage::Ready,
                }),
                "model_lifecycle",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(event.name(), expected, "name() mismatch");
            let env = EventEnvelope::new(0, None, event);
            let wire: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
            assert_eq!(wire["event"], expected, "wire tag mismatch");
            assert_eq!(env.event_name(), expected);
        }
    }

    #[test]
    fn session_update_variants_round_trip() {
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::AgentMessageChunk {
                text: "chunk".to_owned(),
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::ToolCall {
                tool_call_id: "c1".to_owned(),
                title: "read file".to_owned(),
                status: ToolCallStatus::Pending,
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::ToolCallUpdate {
                tool_call_id: "c1".to_owned(),
                status: ToolCallStatus::Completed,
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::Diff {
                path: "src/a.rs".to_owned(),
                old_text: None,
                new_text: "fn a() {}".to_owned(),
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::Plan {
                entries: vec![PlanEntry {
                    content: "write tests".to_owned(),
                    status: PlanEntryStatus::InProgress,
                }],
            },
        });
    }

    #[test]
    fn route_decided_round_trips() {
        round_trip(&RouteDecided {
            phase: Some(Phase::Implement),
            provider_id: ProviderId::from("deepseek"),
            model: Some("deepseek-coder".to_owned()),
            reason: "implement phase routes to the configured cheap tier".to_owned(),
        });
    }

    #[test]
    fn privacy_block_round_trips() {
        round_trip(&PrivacyBlock {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
        });
    }

    #[test]
    fn cost_recorded_round_trips() {
        round_trip(&CostRecorded {
            record: CostRecord {
                session_id: SessionId::from("s1"),
                phase: Some(Phase::Review),
                provider_id: ProviderId::from("anthropic"),
                model: "claude-opus".to_owned(),
                input_tokens: 1000,
                output_tokens: 500,
                usd_micros: 45_000,
            },
        });
    }

    #[test]
    fn provider_degraded_round_trips() {
        round_trip(&ProviderDegraded {
            provider_id: ProviderId::from("flaky"),
            failure_class: FailureClass::ToolCallFailure,
            fallback_id: Some(ProviderId::from("anthropic")),
        });
    }

    #[test]
    fn model_lifecycle_stages_round_trip() {
        for stage in [
            ModelLifecycleStage::Probed {
                ram_bytes: 16 * 1024 * 1024 * 1024,
                above_floor: true,
            },
            ModelLifecycleStage::Download {
                downloaded_bytes: 100,
                total_bytes: Some(1000),
            },
            ModelLifecycleStage::Benchmark {
                first_token_ms: 250,
                tokens_per_sec: 42.5,
            },
            ModelLifecycleStage::Ready,
            ModelLifecycleStage::SteppedDown {
                from_model: "7b".to_owned(),
                to_model: "3b".to_owned(),
                reason: "benchmark exceeded the 1s latency duty".to_owned(),
            },
            ModelLifecycleStage::Disabled {
                reason: "machine below the 8GB floor; running remote-only".to_owned(),
            },
        ] {
            round_trip(&ModelLifecycle {
                model_id: "qwen2.5-coder-3b".to_owned(),
                stage,
            });
        }
    }

    #[test]
    fn permission_request_round_trips() {
        round_trip(&PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: "shell".to_owned(),
            description: Some("run `cargo test`".to_owned()),
            options: vec![
                PermissionOption {
                    option_id: "allow_once".to_owned(),
                    label: "Allow once".to_owned(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "reject_always".to_owned(),
                    label: "Reject for session".to_owned(),
                    kind: PermissionOptionKind::RejectAlways,
                },
            ],
        });
    }

    #[test]
    fn phase_transition_round_trips() {
        round_trip(&PhaseTransition {
            from_phase: Some(Phase::Architect),
            to_phase: Phase::Implement,
            artifacts: vec![TaskArtifactRef {
                req_id: "REQ-544".to_owned(),
                phase: Phase::Architect,
                path: ".adlc/specs/REQ-544/architecture.md".to_owned(),
            }],
        });
    }

    #[test]
    fn daemon_client_attach_round_trips() {
        round_trip(&DaemonClientAttach {
            client_kind: ClientKind::Extension,
            protocol_version: crate::PROTOCOL_VERSION,
        });
    }

    #[test]
    fn envelope_omits_session_id_when_daemon_scoped() {
        let env = EventEnvelope::new(
            9,
            None,
            Event::ModelLifecycle(ModelLifecycle {
                model_id: "qwen".to_owned(),
                stage: ModelLifecycleStage::Ready,
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("session_id"));
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn unknown_fields_in_event_payloads_are_tolerated() {
        // Forward compatibility: an extra field the daemon added later must not
        // break an older client parsing the flattened envelope.
        let json = r#"{
            "session_id": "s1",
            "seq": 4,
            "event": "route_decided",
            "provider_id": "anthropic",
            "reason": "spec phase routes to the frontier tier",
            "future_field": {"weight": 0.9}
        }"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.event_name(), "route_decided");
        match env.event {
            Event::RouteDecided(rd) => assert_eq!(rd.provider_id, ProviderId::from("anthropic")),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
