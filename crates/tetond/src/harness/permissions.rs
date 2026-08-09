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

use teton_core::config::WebTier;
use teton_protocol::events::{
    Event, PermissionOption, PermissionOptionKind, PermissionRequest, WebConsentDecided,
    WebConsentScope, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::PermissionOutcome;
use teton_protocol::{RequestId, SessionId};

use crate::broadcast::EventBus;
use crate::egress::to_protocol_web_tier;
use crate::harness::tools::web::{tier_name, PERMISSION_KEY_FETCH, PERMISSION_KEY_SEARCH};

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

/// Where an `enable_permanent` answer is written down (REQ-563 BR-4).
///
/// A seam rather than a `DaemonRuntime` handle because this module is the
/// permission *model* and owns no configuration: it knows that one answer is
/// durable and four are not, and nothing about where durable lives. The daemon
/// implements it over its atomic config write; a test implements it over a cell
/// and can assert what the gate asked for without a file.
///
/// The `Result` is load-bearing, not decoration. "Permanent" is a claim made to
/// a user, and a persistence that did not happen must not be reported as one —
/// the gate downgrades the recorded scope to
/// [`WebConsentScope::Session`] when this answers `Err`, which is the honest
/// description of what the answer then bought.
pub trait WebTierPersistence: Send + Sync {
    /// Persist `tier` as the configured web ceiling.
    ///
    /// # Errors
    /// A human-readable sentence naming what stopped the write.
    fn persist_web_tier(&self, tier: WebTier) -> Result<(), String>;
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
    /// Where `enable_permanent` writes, when anything offers it (REQ-563 BR-4).
    ///
    /// `None` on a gate nobody wired one into: the option is still offered and
    /// still allows, and the decision is recorded at the scope it actually
    /// achieved. An unwired sink is a gate that cannot promise permanence, not a
    /// gate that lies about it.
    web_persistence: Option<Arc<dyn WebTierPersistence>>,
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
            web_persistence: None,
        }
    }

    /// Wire the sink `enable_permanent` writes through (REQ-563 BR-4).
    #[must_use]
    pub fn with_web_persistence(mut self, sink: Arc<dyn WebTierPersistence>) -> Self {
        self.web_persistence = Some(sink);
        self
    }

    /// Decide whether `tool_name` may run, prompting the client if the policy is
    /// `ask` and no session grant already answers.
    ///
    /// A cancelled prompt, a `reject_*`, or a dropped client (channel closed) all
    /// resolve to [`PermissionDecision::Denied`] — the safe default.
    ///
    /// Web lookups do not come through here: they carry a tier, and
    /// [`Self::authorize_web`] is the entry point that takes one.
    pub async fn authorize(
        &self,
        tool_name: &str,
        description: Option<String>,
    ) -> PermissionDecision {
        // A web key arriving without its tier would be offered the permanent
        // option and then be unable to say which tier it made permanent. The
        // types cannot forbid it (the key is a `&str`), so it is asserted where
        // it would happen rather than left to be found in a consent event with a
        // missing tier.
        debug_assert!(
            !is_web_permission_key(tool_name),
            "`{tool_name}` is a web tier key and must be authorized through \
             `authorize_web`, which carries the tier the decision is about"
        );
        self.decide(tool_name, description, None).await
    }

    /// Decide whether a web lookup at `tier` may run (REQ-563 BR-3/BR-4).
    ///
    /// Two things separate this from [`Self::authorize`], and both come from the
    /// tier being in hand:
    ///
    /// - the prompt offers a fifth option, `enable_permanent`, which writes the
    ///   tier to config (BR-4 makes that the only consent path that writes
    ///   anything);
    /// - whatever the user answers is published as a
    ///   [`WebConsentDecided`] event, at the scope the answer actually achieved.
    ///
    /// `permission_key` stays separate from `tier` because they are different
    /// questions with different arities: two keys (`web_fetch`, `web_search`)
    /// carry the session grant, three tiers can be made permanent. A fetch of a
    /// URL the user pasted and a fetch of one the model chose share a grant key
    /// and do **not** share a tier — collapsing them would make consenting to
    /// one silently write the other.
    pub async fn authorize_web(
        &self,
        permission_key: &str,
        description: Option<String>,
        tier: WebTier,
    ) -> PermissionDecision {
        self.decide(permission_key, description, Some(tier)).await
    }

    /// The shared body of both entry points; `web` carries the tier when this is
    /// a web-lookup decision and is `None` for every other tool.
    async fn decide(
        &self,
        tool_name: &str,
        description: Option<String>,
        web: Option<WebTier>,
    ) -> PermissionDecision {
        // A remembered session grant short-circuits everything (asked once).
        //
        // No consent event here, deliberately: the decision this replays was
        // published when it was *made*, and re-announcing it per lookup would
        // turn one decision into a stream of them.
        if let Some(grant) = self.session_grant(tool_name) {
            return match grant {
                Grant::AllowAlways => PermissionDecision::Allowed,
                Grant::RejectAlways => PermissionDecision::Denied,
            };
        }

        // Likewise nothing is published for a policy answer: `allow` and `deny`
        // rows are configuration, and no one decided anything just now.
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
                options: options_for(web),
            }),
        );

        match rx.await {
            Ok(outcome) => self.interpret(tool_name, outcome, web),
            // Client disconnected before answering: deny (never run unapproved).
            // Not a consent decision — nobody decided it — so nothing is
            // published; a `web_consent_decided { granted: false }` here would
            // record a refusal the user never gave.
            Err(_) => PermissionDecision::Denied,
        }
    }

    /// Interpret a client's chosen option, recording any `*_always` grant and —
    /// for a web decision — publishing it at the scope it achieved.
    fn interpret(
        &self,
        tool_name: &str,
        outcome: PermissionOutcome,
        web: Option<WebTier>,
    ) -> PermissionDecision {
        let (decision, scope) = match outcome {
            PermissionOutcome::Selected { option_id } => match option_id.as_str() {
                OPTION_ALLOW_ONCE => (PermissionDecision::Allowed, WebConsentScope::Once),
                OPTION_ALLOW_ALWAYS => {
                    self.remember(tool_name, Grant::AllowAlways);
                    (PermissionDecision::Allowed, WebConsentScope::Session)
                }
                // Only reachable when the option was offered, which is only when
                // a tier is in hand: an id that was not on the prompt is not an
                // answer to it, and falls through to the deny arm below.
                OPTION_ID_ENABLE_PERMANENT if web.is_some() => {
                    // The session grant is recorded whether or not the write
                    // lands: the user said yes to this session either way, and
                    // making the answer contingent on a filesystem would re-ask
                    // a question already answered.
                    self.remember(tool_name, Grant::AllowAlways);
                    let scope = self.persist_web_tier(web.unwrap_or(WebTier::Off));
                    (PermissionDecision::Allowed, scope)
                }
                OPTION_REJECT_ALWAYS => {
                    self.remember(tool_name, Grant::RejectAlways);
                    (PermissionDecision::Denied, WebConsentScope::Session)
                }
                // reject_once and any unknown id: deny this once.
                _ => (PermissionDecision::Denied, WebConsentScope::Once),
            },
            PermissionOutcome::Cancelled => (PermissionDecision::Denied, WebConsentScope::Once),
        };

        if let Some(tier) = web {
            self.events.publish(
                Some(self.session_id.clone()),
                Event::WebConsentDecided(WebConsentDecided {
                    scope,
                    tier: to_protocol_web_tier(tier),
                    granted: decision == PermissionDecision::Allowed,
                }),
            );
        }
        decision
    }

    /// Write `tier` through the persistence seam, answering with the scope the
    /// attempt actually achieved.
    ///
    /// A failure is reported to the operator's stderr and downgraded to
    /// [`WebConsentScope::Session`] rather than turned into a denial: the user
    /// said yes, and the only thing that did not happen is the part that would
    /// have outlived the session. The line names the reason so "why am I being
    /// asked again next time" has an answer on this machine.
    fn persist_web_tier(&self, tier: WebTier) -> WebConsentScope {
        let Some(sink) = &self.web_persistence else {
            eprintln!(
                "teton: web lookup was enabled for this session only — this daemon has no \
                 configured place to record the choice permanently."
            );
            return WebConsentScope::Session;
        };
        match sink.persist_web_tier(tier) {
            Ok(()) => WebConsentScope::Persistent,
            Err(err) => {
                eprintln!(
                    "teton: web lookup was enabled for this session only — the choice could \
                     not be made permanent ({err})."
                );
                WebConsentScope::Session
            }
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

/// Whether `tool_name` is one of the web tiers' consent keys (REQ-563 BR-3).
///
/// Two keys and not one: a grant is remembered under exactly the string it was
/// asked about, so `web` as a single key would have made one "allow for this
/// session" on a page fetch silently grant every search.
#[must_use]
pub fn is_web_permission_key(tool_name: &str) -> bool {
    tool_name == PERMISSION_KEY_FETCH || tool_name == PERMISSION_KEY_SEARCH
}

/// The options offered on a prompt: the four standard ones, plus the persistent
/// enable when `web` names the tier a decision could be written down at.
///
/// Keyed on the tier rather than on the tool name, so the option and the value
/// it would persist come from **one** source: an `enable_permanent` that could
/// be offered without a tier in hand is an option the daemon could not honour.
/// That is also why the fifth option is web-only rather than universal — there
/// is no `[shell] tier` to write, and an "always" that quietly edited config
/// would be a much larger promise than the one the prompt makes.
fn options_for(web: Option<WebTier>) -> Vec<PermissionOption> {
    let mut options = vec![
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
    ];
    if let Some(tier) = web {
        // Named in the label, because BR-4 wants consent concrete: "enable
        // permanently" alone does not say what is being enabled, and the two
        // fetch tiers are a distinction a user has to be able to see.
        options.push(PermissionOption {
            option_id: OPTION_ID_ENABLE_PERMANENT.to_owned(),
            label: format!(
                "Enable permanently (writes `[web] tier = \"{}\"` to your config)",
                tier_name(tier)
            ),
            kind: PermissionOptionKind::AllowAlways,
        });
    }
    options.push(PermissionOption {
        option_id: OPTION_REJECT_ONCE.to_owned(),
        label: "Reject once".to_owned(),
        kind: PermissionOptionKind::RejectOnce,
    });
    options.push(PermissionOption {
        option_id: OPTION_REJECT_ALWAYS.to_owned(),
        label: "Reject for this session".to_owned(),
        kind: PermissionOptionKind::RejectAlways,
    });
    options
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

    // ------------------------------------------------------------------
    // REQ-563: the fifth option, and the consent event (BR-4, AC-2)
    // ------------------------------------------------------------------

    /// A [`WebTierPersistence`] that records what it was asked to write and can
    /// be told to fail, so both halves of the "permanent" claim are testable
    /// without a config file.
    #[derive(Default)]
    struct RecordingSink {
        written: Mutex<Vec<WebTier>>,
        fails: bool,
    }

    impl RecordingSink {
        fn failing() -> Self {
            Self {
                written: Mutex::new(Vec::new()),
                fails: true,
            }
        }

        fn written(&self) -> Vec<WebTier> {
            self.written.lock().expect("sink mutex").clone()
        }
    }

    impl WebTierPersistence for RecordingSink {
        fn persist_web_tier(&self, tier: WebTier) -> Result<(), String> {
            self.written.lock().expect("sink mutex").push(tier);
            if self.fails {
                return Err("the disk said no".to_owned());
            }
            Ok(())
        }
    }

    /// Drive one Ask prompt to `option_id`, returning the decision, the options
    /// the prompt carried, and every event the bus saw.
    async fn answer_web(
        gate: &PermissionGate,
        bus: &Arc<EventBus>,
        pending: &Arc<PendingPermissions>,
        key: &str,
        tier: WebTier,
        option_id: &str,
    ) -> (PermissionDecision, Vec<PermissionOption>, Vec<Event>) {
        let mut sub = bus.subscribe(16);
        let decide = gate.authorize_web(key, Some("fetch https://docs.rs".to_owned()), tier);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let (rid, options) = match env.event {
                Event::PermissionRequest(pr) => (pr.request_id, pr.options),
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: option_id.to_owned(),
                },
            );
            options
        };
        let (decision, options) = tokio::join!(decide, drive);
        let mut rest = Vec::new();
        while let Some(env) = sub.try_recv() {
            rest.push(env.event);
        }
        (decision, options, rest)
    }

    /// AC-2's option list, and its negative half: the persistent choice belongs
    /// to the web tiers alone. `shell` and `edit` keep exactly four, because a
    /// consent answer that quietly edited config would be a far larger promise
    /// than "allow for this session".
    #[tokio::test]
    async fn only_the_web_keys_are_offered_the_persistent_option() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));

        for key in [PERMISSION_KEY_FETCH, PERMISSION_KEY_SEARCH] {
            let (_, options, _) = answer_web(
                &gate,
                &bus,
                &pending,
                key,
                WebTier::FetchAnyUrl,
                OPTION_REJECT_ONCE,
            )
            .await;
            let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    OPTION_ALLOW_ONCE,
                    OPTION_ALLOW_ALWAYS,
                    OPTION_ID_ENABLE_PERMANENT,
                    OPTION_REJECT_ONCE,
                    OPTION_REJECT_ALWAYS,
                ],
                "`{key}` must offer the five web options, in prompt order"
            );
            assert!(
                options
                    .iter()
                    .any(|o| o.option_id == OPTION_ID_ENABLE_PERMANENT
                        && o.label.contains("fetch_any_url")),
                "the label must name the tier it would write: {options:?}"
            );
        }

        // The negative half, driven through the same publisher so this is the
        // prompt a client actually receives rather than a helper's return value.
        for tool in ["shell", "edit", "read"] {
            let mut sub = bus.subscribe(16);
            let decide = gate.authorize(tool, None);
            let drive = async {
                let env = sub.recv().await.unwrap();
                let (rid, options) = match env.event {
                    Event::PermissionRequest(pr) => (pr.request_id, pr.options),
                    other => panic!("expected permission_request, got {other:?}"),
                };
                pending.resolve(&rid, PermissionOutcome::Cancelled);
                options
            };
            let (_, options) = tokio::join!(decide, drive);
            let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    OPTION_ALLOW_ONCE,
                    OPTION_ALLOW_ALWAYS,
                    OPTION_REJECT_ONCE,
                    OPTION_REJECT_ALWAYS,
                ],
                "`{tool}` is not a web tier and must keep exactly four options"
            );
        }
    }

    /// `enable_permanent` writes the tier it was offered for — and the tier is
    /// the *lookup's*, not the permission key's: `web_fetch` covers two tiers,
    /// and consenting to a user-pasted-URL fetch must not enable model-chosen
    /// ones (BR-3).
    #[tokio::test]
    async fn enable_permanent_writes_the_tier_the_prompt_named() {
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            let bus = Arc::new(EventBus::new());
            let pending = Arc::new(PendingPermissions::new());
            let sink = Arc::new(RecordingSink::default());
            let gate = PermissionGate::new(
                SessionId::from("s1"),
                PermissionConfig::with_default(PermissionPolicy::Ask),
                Arc::clone(&bus),
                Arc::clone(&pending),
            )
            .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);

            let key = if tier == WebTier::Search {
                PERMISSION_KEY_SEARCH
            } else {
                PERMISSION_KEY_FETCH
            };
            let (decision, _, events) =
                answer_web(&gate, &bus, &pending, key, tier, OPTION_ID_ENABLE_PERMANENT).await;

            assert_eq!(decision, PermissionDecision::Allowed);
            assert_eq!(sink.written(), vec![tier], "wrote the wrong tier");
            assert_eq!(
                events,
                vec![Event::WebConsentDecided(WebConsentDecided {
                    scope: WebConsentScope::Persistent,
                    tier: to_protocol_web_tier(tier),
                    granted: true,
                })],
                "a persisted grant is recorded at persistent scope"
            );

            // And it is a session grant thereafter: the next lookup at the same
            // key asks nobody and writes nothing more.
            assert_eq!(
                gate.authorize_web(key, None, tier).await,
                PermissionDecision::Allowed
            );
            assert_eq!(
                sink.written(),
                vec![tier],
                "the remembered grant must not re-write config on every lookup"
            );
        }
    }

    /// A write that does not land is reported as a **session** grant, not a
    /// persistent one. The user still said yes — so the lookup proceeds — but
    /// the event must not claim a durability that does not exist.
    #[tokio::test]
    async fn a_failed_write_is_recorded_as_a_session_grant_not_a_permanent_one() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let sink = Arc::new(RecordingSink::failing());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            PermissionConfig::with_default(PermissionPolicy::Ask),
            Arc::clone(&bus),
            Arc::clone(&pending),
        )
        .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);

        let (decision, _, events) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_SEARCH,
            WebTier::Search,
            OPTION_ID_ENABLE_PERMANENT,
        )
        .await;

        assert_eq!(
            decision,
            PermissionDecision::Allowed,
            "a filesystem failure must not overturn the user's answer"
        );
        assert_eq!(
            events,
            vec![Event::WebConsentDecided(WebConsentDecided {
                scope: WebConsentScope::Session,
                tier: to_protocol_web_tier(WebTier::Search),
                granted: true,
            })]
        );
    }

    /// A gate with no persistence seam still allows, and still tells the truth
    /// about the scope it achieved.
    #[tokio::test]
    async fn enable_permanent_without_a_sink_degrades_to_a_session_grant() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
        let (decision, _, events) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_FETCH,
            WebTier::FetchAnyUrl,
            OPTION_ID_ENABLE_PERMANENT,
        )
        .await;
        assert_eq!(decision, PermissionDecision::Allowed);
        assert_eq!(
            events,
            vec![Event::WebConsentDecided(WebConsentDecided {
                scope: WebConsentScope::Session,
                tier: to_protocol_web_tier(WebTier::FetchAnyUrl),
                granted: true,
            })]
        );
    }

    /// Every web answer is recorded, refusals included, at the scope it refuses
    /// — "declined this once" and "declined for the session" decide whether the
    /// user is asked again.
    #[tokio::test]
    async fn every_web_answer_is_recorded_at_the_scope_it_holds() {
        let cases = [
            (OPTION_ALLOW_ONCE, WebConsentScope::Once, true),
            (OPTION_ALLOW_ALWAYS, WebConsentScope::Session, true),
            (OPTION_REJECT_ONCE, WebConsentScope::Once, false),
            (OPTION_REJECT_ALWAYS, WebConsentScope::Session, false),
        ];
        for (option_id, scope, granted) in cases {
            let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
            let (decision, _, events) = answer_web(
                &gate,
                &bus,
                &pending,
                PERMISSION_KEY_FETCH,
                WebTier::FetchUserUrl,
                option_id,
            )
            .await;
            assert_eq!(
                decision == PermissionDecision::Allowed,
                granted,
                "{option_id} decided the wrong way"
            );
            assert_eq!(
                events,
                vec![Event::WebConsentDecided(WebConsentDecided {
                    scope,
                    tier: to_protocol_web_tier(WebTier::FetchUserUrl),
                    granted,
                })],
                "{option_id} was recorded wrongly"
            );
        }
    }

    /// An option id that was never on the prompt is not an answer to it. In
    /// particular `enable_permanent` sent for a **non-web** tool denies rather
    /// than granting — the fail-closed default that keeps a client (or a
    /// replayed message) from reaching an option the daemon did not offer.
    #[tokio::test]
    async fn an_unoffered_option_id_denies() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let sink = Arc::new(RecordingSink::default());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            PermissionConfig::with_default(PermissionPolicy::Ask),
            Arc::clone(&bus),
            Arc::clone(&pending),
        )
        .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: OPTION_ID_ENABLE_PERMANENT.to_owned(),
                },
            );
        };
        let (decision, ()) = tokio::join!(decide, drive);

        assert_eq!(decision, PermissionDecision::Denied);
        assert!(
            sink.written().is_empty(),
            "a non-web prompt must never reach the config writer"
        );
        assert!(
            sub.try_recv().is_none(),
            "a non-web decision publishes no web consent event"
        );
    }

    /// A remembered grant replays without re-announcing itself: one decision is
    /// one event, however many lookups it later answers.
    #[tokio::test]
    async fn a_replayed_session_grant_publishes_no_second_decision() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
        let (_, _, first) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_FETCH,
            WebTier::FetchAnyUrl,
            OPTION_ALLOW_ALWAYS,
        )
        .await;
        assert_eq!(first.len(), 1);

        let mut sub = bus.subscribe(16);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_FETCH, None, WebTier::FetchAnyUrl)
                .await,
            PermissionDecision::Allowed
        );
        assert!(
            sub.try_recv().is_none(),
            "the replayed grant re-announced a decision nobody made"
        );
    }

    /// A policy row answers without a prompt and without a decision event: an
    /// `allow`/`deny` row is configuration, and nobody decided anything now.
    #[tokio::test]
    async fn a_policy_answer_publishes_no_consent_event() {
        let (bus, _pending, gate) = gate(PermissionConfig::permissive());
        let mut sub = bus.subscribe(16);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_FETCH, None, WebTier::FetchAnyUrl)
                .await,
            PermissionDecision::Allowed
        );
        assert!(sub.try_recv().is_none());
    }
}
