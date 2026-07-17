//! The tool permission model: per-tool policy, a client round-trip, and
//! session-scoped grants.
//!
//! Modeled on Claude Code's allowlist-plus-prompt (spec Permissions table). Each
//! tool has a policy:
//!
//! - **allow** — run without asking (read-only tools by default),
//! - **deny** — never run,
//! - **ask** — emit a [`permission_request`](teton_protocol::events::PermissionRequest)
//!   event and wait for a client to answer.
//!
//! The round-trip uses the daemon's real machinery: the request goes out over
//! TASK-004's [`EventBus`], and the client's reply arrives — in a later task's
//! server wiring — as a `permission/respond` method that calls
//! [`PendingPermissions::resolve`]. That call is the seam; this module owns
//! everything up to it.
//!
//! A `*_always` answer is remembered for the **session only** ([`PermissionGate`]
//! holds the grants), so the user is asked once per tool per session and never
//! persisted to disk.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use teton_protocol::events::{Event, PermissionOption, PermissionOptionKind, PermissionRequest};
use teton_protocol::methods::PermissionOutcome;
use teton_protocol::{RequestId, SessionId};

use crate::broadcast::EventBus;

/// Policy for a single tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Run without prompting.
    Allow,
    /// Prompt the client and wait for an answer.
    Ask,
    /// Never run.
    Deny,
}

/// The resolved decision for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// The call may proceed.
    Allowed,
    /// The call is cancelled; the model is told and must not retry it.
    Denied,
}

/// A remembered, session-scoped answer for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// Always allow for the rest of the session.
    AllowAlways,
    /// Always reject for the rest of the session.
    RejectAlways,
}

/// The per-tool policy table.
#[derive(Debug, Clone)]
pub struct PermissionConfig {
    default: PermissionPolicy,
    per_tool: HashMap<String, PermissionPolicy>,
}

impl PermissionConfig {
    /// A config with the given default and no per-tool overrides.
    #[must_use]
    pub fn with_default(default: PermissionPolicy) -> Self {
        Self {
            default,
            per_tool: HashMap::new(),
        }
    }

    /// Sensible coding defaults: read-only tools auto-allow, mutating tools ask.
    #[must_use]
    pub fn coding_defaults() -> Self {
        let mut cfg = Self::with_default(PermissionPolicy::Ask);
        cfg.set("read", PermissionPolicy::Allow);
        cfg.set("glob", PermissionPolicy::Allow);
        cfg.set("grep", PermissionPolicy::Allow);
        cfg.set("edit", PermissionPolicy::Ask);
        cfg.set("shell", PermissionPolicy::Ask);
        cfg
    }

    /// A config that allows every tool (used by the offline demo path where the
    /// operator has pre-approved the local, jailed tool set).
    #[must_use]
    pub fn permissive() -> Self {
        Self::with_default(PermissionPolicy::Allow)
    }

    /// Override the policy for a tool.
    pub fn set(&mut self, tool: impl Into<String>, policy: PermissionPolicy) {
        self.per_tool.insert(tool.into(), policy);
    }

    /// The policy that applies to `tool`.
    #[must_use]
    pub fn policy_for(&self, tool: &str) -> PermissionPolicy {
        self.per_tool.get(tool).copied().unwrap_or(self.default)
    }
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self::coding_defaults()
    }
}

/// The registry of in-flight permission prompts, keyed by request id.
///
/// The harness registers a waiter here and awaits it; a client's
/// `permission/respond` (wired in a later task) calls [`Self::resolve`]. Kept
/// separate from [`PermissionGate`] because it is daemon-wide (one client reply
/// must find the waiter regardless of which session raised it), whereas grants
/// are per-session.
#[derive(Default)]
pub struct PendingPermissions {
    waiters: Mutex<HashMap<RequestId, oneshot::Sender<PermissionOutcome>>>,
}

impl PendingPermissions {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter and return the receiver the caller awaits.
    fn register(&self, id: RequestId) -> oneshot::Receiver<PermissionOutcome> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .insert(id, tx);
        rx
    }

    /// Deliver a client's answer to the waiting harness. Returns `true` if a
    /// waiter was present. This is the entry point the server's
    /// `permission/respond` handler calls.
    pub fn resolve(&self, id: &RequestId, outcome: PermissionOutcome) -> bool {
        let sender = self
            .waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .remove(id);
        match sender {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Number of prompts currently awaiting an answer.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .len()
    }
}

/// The session-scoped permission authority.
///
/// Publishes prompts to the event bus, awaits answers via [`PendingPermissions`],
/// and remembers `*_always` answers for the life of the session.
pub struct PermissionGate {
    session_id: SessionId,
    config: PermissionConfig,
    grants: Mutex<HashMap<String, Grant>>,
    events: Arc<EventBus>,
    pending: Arc<PendingPermissions>,
    counter: AtomicU64,
}

impl PermissionGate {
    /// A gate for `session_id` using `config`, publishing to `events` and
    /// awaiting answers on `pending`.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        config: PermissionConfig,
        events: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
    ) -> Self {
        Self {
            session_id,
            config,
            grants: Mutex::new(HashMap::new()),
            events,
            pending,
            counter: AtomicU64::new(0),
        }
    }

    /// Decide whether `tool_name` may run, prompting the client if the policy is
    /// `ask` and no session grant already answers.
    ///
    /// A cancelled prompt, a `reject_*`, or a dropped client (channel closed) all
    /// resolve to [`PermissionDecision::Denied`] — the safe default.
    pub async fn authorize(
        &self,
        tool_name: &str,
        description: Option<String>,
    ) -> PermissionDecision {
        // A remembered session grant short-circuits everything (asked once).
        if let Some(grant) = self.session_grant(tool_name) {
            return match grant {
                Grant::AllowAlways => PermissionDecision::Allowed,
                Grant::RejectAlways => PermissionDecision::Denied,
            };
        }

        match self.config.policy_for(tool_name) {
            PermissionPolicy::Allow => return PermissionDecision::Allowed,
            PermissionPolicy::Deny => return PermissionDecision::Denied,
            PermissionPolicy::Ask => {}
        }

        // Register the waiter, publish the prompt, then await — no lock is held
        // across the await.
        let request_id = RequestId::from(format!(
            "perm-{}",
            self.counter.fetch_add(1, Ordering::SeqCst)
        ));
        let rx = self.pending.register(request_id.clone());

        self.events.publish(
            Some(self.session_id.clone()),
            Event::PermissionRequest(PermissionRequest {
                request_id,
                tool_name: tool_name.to_owned(),
                description,
                options: standard_options(),
            }),
        );

        match rx.await {
            Ok(outcome) => self.interpret(tool_name, outcome),
            // Client disconnected before answering: deny (never run unapproved).
            Err(_) => PermissionDecision::Denied,
        }
    }

    /// Interpret a client's chosen option, recording any `*_always` grant.
    fn interpret(&self, tool_name: &str, outcome: PermissionOutcome) -> PermissionDecision {
        match outcome {
            PermissionOutcome::Selected { option_id } => match option_id.as_str() {
                OPTION_ALLOW_ONCE => PermissionDecision::Allowed,
                OPTION_ALLOW_ALWAYS => {
                    self.remember(tool_name, Grant::AllowAlways);
                    PermissionDecision::Allowed
                }
                OPTION_REJECT_ALWAYS => {
                    self.remember(tool_name, Grant::RejectAlways);
                    PermissionDecision::Denied
                }
                // reject_once and any unknown id: deny this once.
                _ => PermissionDecision::Denied,
            },
            PermissionOutcome::Cancelled => PermissionDecision::Denied,
        }
    }

    fn session_grant(&self, tool_name: &str) -> Option<Grant> {
        self.grants
            .lock()
            .expect("permission grants mutex poisoned")
            .get(tool_name)
            .copied()
    }

    fn remember(&self, tool_name: &str, grant: Grant) {
        self.grants
            .lock()
            .expect("permission grants mutex poisoned")
            .insert(tool_name.to_owned(), grant);
    }
}

const OPTION_ALLOW_ONCE: &str = "allow_once";
const OPTION_ALLOW_ALWAYS: &str = "allow_always";
const OPTION_REJECT_ONCE: &str = "reject_once";
const OPTION_REJECT_ALWAYS: &str = "reject_always";

/// The four standard options offered on every prompt (ACP `PermissionOption`s).
fn standard_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: OPTION_ALLOW_ONCE.to_owned(),
            label: "Allow once".to_owned(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: OPTION_ALLOW_ALWAYS.to_owned(),
            label: "Allow for this session".to_owned(),
            kind: PermissionOptionKind::AllowAlways,
        },
        PermissionOption {
            option_id: OPTION_REJECT_ONCE.to_owned(),
            label: "Reject once".to_owned(),
            kind: PermissionOptionKind::RejectOnce,
        },
        PermissionOption {
            option_id: OPTION_REJECT_ALWAYS.to_owned(),
            label: "Reject for this session".to_owned(),
            kind: PermissionOptionKind::RejectAlways,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(config: PermissionConfig) -> (Arc<EventBus>, Arc<PendingPermissions>, PermissionGate) {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            config,
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        (bus, pending, gate)
    }

    #[tokio::test]
    async fn allow_policy_needs_no_prompt() {
        let (bus, _pending, gate) = gate(PermissionConfig::permissive());
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Allowed
        );
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn deny_policy_needs_no_prompt() {
        let mut cfg = PermissionConfig::with_default(PermissionPolicy::Deny);
        cfg.set("shell", PermissionPolicy::Deny);
        let (_bus, _pending, gate) = gate(cfg);
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Denied
        );
    }

    #[tokio::test]
    async fn ask_then_reject_always_denies_and_persists_for_the_session() {
        let mut cfg = PermissionConfig::permissive();
        cfg.set("shell", PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", Some("run tests".to_owned()));
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            assert!(pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: "reject_always".to_owned()
                }
            ));
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Denied);

        // Second call: the reject_always grant answers with no new prompt.
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Denied
        );
        assert_eq!(pending.pending_count(), 0);
    }

    #[tokio::test]
    async fn ask_then_allow_always_allows_and_persists() {
        let cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("edit", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: "allow_always".to_owned(),
                },
            );
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Allowed);

        // Persisted: allowed again with no prompt.
        assert_eq!(
            gate.authorize("edit", None).await,
            PermissionDecision::Allowed
        );
    }

    #[tokio::test]
    async fn cancelled_prompt_denies() {
        let mut cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
        cfg.set("shell", PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                _ => unreachable!(),
            };
            pending.resolve(&rid, PermissionOutcome::Cancelled);
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Denied);
    }
}
