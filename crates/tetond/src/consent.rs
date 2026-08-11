//! Attach and monitor consent: the one thing in the daemon that mints a grant.
//!
//! [`crate::grants`] decides whether a connection *holds* standing. This module
//! is how standing is *acquired* — and it is deliberately the only way (BR-3,
//! ADR-E). A connection that neither created a session nor holds a grant for it
//! does not get a refusal any more; it gets a question put to a user, and the
//! answer either mints exactly one grant or mints nothing at all.
//!
//! ## The shape, borrowed; the plumbing, not (ADR-E)
//!
//! This mirrors the proven [`crate::harness::permissions::PendingPermissions`]
//! pattern — publish an event carrying a request id, await a `oneshot`, resolve
//! on an RPC reply, default closed on timeout or drop — but it is its **own**
//! registry and its own method. Reusing the permission registry would have been
//! wrong twice over: it is session-scoped by construction and an attach request
//! by definition has no attachment yet, and `permission/respond` is itself a
//! gated method (BR-9), so routing consent through it would put the gate in
//! front of the thing that opens the gate.
//!
//! ## Three parts, and only the middle one holds state
//!
//! - [`ConsentRoute`] is **pure policy**: who may render a prompt, and
//!   therefore who may answer it. Plain values, no locks, no channels — so
//!   BR-6's routing rule is table-tested directly rather than inferred from
//!   what a handler happened to deliver (LESSON-499).
//! - [`PendingConsents`] is the waiter registry: daemon-wide monotonic ids,
//!   one `oneshot` per request, and a `register` that refuses to overwrite.
//! - [`ConsentSurfaces`] is the delivery mechanism: which live connections can
//!   be reached, and their outbound channels.
//!
//! ## Ids are minted where they are resolved (LESSON-503)
//!
//! The counter lives on [`PendingConsents`] — the daemon-wide map that
//! *resolves* the id — never on anything per-session or per-connection. That is
//! BUG-161's whole lesson: an id minted in a narrower scope than the one it is
//! looked up in collides, and a colliding consent id would let one request's
//! answer mint another request's grant.
//!
//! ## Nothing here is persisted (ADR-C)
//!
//! A pending consent dies with the daemon, and a decision dies with the
//! connection it was made for. There is no file, no keychain entry, and no
//! "remember this answer" — the entire point of the control is that possession
//! of local state must not confer access (BR-3).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use teton_protocol::{RequestId, SessionId};

use crate::grants::ConnectionId;

/// How long a consent request waits for an answer before it defaults closed
/// (REQ-569 BR-7, AC-6).
///
/// Thirty seconds: long enough for a user to notice a prompt on a surface they
/// were not looking at and answer it, short enough that an unanswered request
/// cannot hold an attach — and a client — waiting on a decision nobody is going
/// to make.
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How a consent request ended.
///
/// Three outcomes, and two of them mint nothing. [`Denied`](Self::Denied) and
/// [`TimedOut`](Self::TimedOut) are kept apart all the way out to the wire
/// because they have different remedies (BR-5): a denial was a decision, a
/// timeout was a prompt nobody answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// A user approved it, at the surface described by `approved_at`.
    Granted {
        /// A session the approving surface holds — the session recorded in the
        /// minted [`crate::grants::Grant`]'s key.
        ///
        /// `None` when the requester rendered its own prompt (BR-6's second
        /// arm), where the only grant that can be minted is attach-scope and is
        /// keyed to the session that was asked for anyway.
        approved_at: Option<SessionId>,
    },
    /// A user was asked and said no.
    Denied,
    /// The bounded window elapsed with no answer. Resolves to denied.
    TimedOut,
}

/// Which connections a consent prompt is offered to — and therefore which
/// connections may answer it (BR-6).
///
/// **Pure**, so the routing rule is a table rather than an emergent property of
/// the delivery code, and so the *same* rule answers both questions. Delivering
/// to one set and accepting answers from another would be two rules to drift
/// apart, and the drift would be silent: the surface that renders a prompt is
/// exactly the surface whose user is being asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRoute {
    /// The connection whose request this is. It is part of every route because
    /// two of the three rules are stated in terms of it, and because the
    /// requester is always told how its own request ended.
    requester: ConnectionId,
    rendered_by: RenderedBy,
}

/// The three routing rules, one per case BR-6 distinguishes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RenderedBy {
    /// **Attach, first arm.** Some connection is already attached to the target
    /// session, so the prompt goes to whoever is — a surface whose user
    /// demonstrably already owns that session.
    ConnectionsAttachedTo(SessionId),
    /// **Attach, second arm.** Nothing is attached to the target session, so
    /// the requester renders its own prompt. This is the resume flow BR-6
    /// requires to keep working when the user's last client has gone: the
    /// person reopening their session is the person being asked.
    ///
    /// Sound only because the ancestry gate ran first (BR-4, ADR-A): every
    /// connection out of the daemon's own process tree — every tool child, every
    /// MCP subprocess — was already refused `ATTACH_FORBIDDEN` and never
    /// reaches this module, so "ask the requester" can never mean "ask the
    /// daemon's own spawned code".
    TheRequesterItself,
    /// **Monitor, and deliberately narrower than either attach arm.** A monitor
    /// is a whole-daemon read capability, so it is approved only at a surface
    /// the user demonstrably already owns — a connection attached to *some*
    /// session — and never by the requester itself. If nobody is attached
    /// anywhere there is no one to ask, and the request is refused rather than
    /// self-approved.
    AnyAttachedPeer,
}

impl ConsentRoute {
    /// BR-6's first arm: rendered by whoever is attached to `session`.
    #[must_use]
    pub fn attached_to(requester: ConnectionId, session: SessionId) -> Self {
        Self {
            requester,
            rendered_by: RenderedBy::ConnectionsAttachedTo(session),
        }
    }

    /// BR-6's second arm: nothing is attached to the target, so the requester
    /// renders its own prompt.
    #[must_use]
    pub fn requester_itself(requester: ConnectionId) -> Self {
        Self {
            requester,
            rendered_by: RenderedBy::TheRequesterItself,
        }
    }

    /// The monitor rule: any attached peer, never the requester.
    #[must_use]
    pub fn any_attached_peer(requester: ConnectionId) -> Self {
        Self {
            requester,
            rendered_by: RenderedBy::AnyAttachedPeer,
        }
    }

    /// The connection that asked.
    #[must_use]
    pub fn requester(&self) -> ConnectionId {
        self.requester
    }

    /// Whether the connection `connection`, attached to `attached`, is offered
    /// this prompt — and may therefore answer it.
    ///
    /// One predicate for delivery *and* authorization. `attach/consent` calls it
    /// on the way in for the same reason the publisher calls it on the way out:
    /// receiving a frame is not standing to answer it, and a `monitor` — which
    /// sees every session's events — must not be able to approve anything by
    /// virtue of having seen the request (LESSON-502).
    #[must_use]
    pub fn renders_request(&self, connection: ConnectionId, attached: &HashSet<SessionId>) -> bool {
        match &self.rendered_by {
            RenderedBy::ConnectionsAttachedTo(session) => attached.contains(session),
            RenderedBy::TheRequesterItself => connection == self.requester,
            RenderedBy::AnyAttachedPeer => connection != self.requester && !attached.is_empty(),
        }
    }

    /// Whether `connection` answering this request is approving **its own**
    /// (BR-6's second arm, REQ-569 TASK-109).
    ///
    /// True only for [`RenderedBy::TheRequesterItself`] answered by the
    /// requester — the one arm in which no second party is involved at all. For
    /// a headless same-UID process that is not a daemon descendant, this is the
    /// accepted, documented residual of ADR-A's perimeter: the requester is
    /// asked, and it answers itself. Accepted, but never *silent* — the daemon
    /// says so in its log ([`crate::server`]'s self-approval line), and this
    /// predicate is what decides when.
    ///
    /// Stated as "the self-render arm **and** the requester" rather than as
    /// either half alone. The arm alone would be a claim about routing, not
    /// about who answered; the requester alone would also match a peer arm that
    /// the requester is (wrongly) allowed to answer — and a rule that names both
    /// keeps the log honest if either half ever changes.
    #[must_use]
    pub fn self_approved_by(&self, connection: ConnectionId) -> bool {
        matches!(self.rendered_by, RenderedBy::TheRequesterItself) && connection == self.requester
    }

    /// Whether `connection` is told how the request *ended*.
    ///
    /// Everyone who was offered the prompt, plus the requester — which is a
    /// superset rather than the same rule on purpose. The requester has to be
    /// able to retire its own pending state, and by construction it is attached
    /// to nothing, so no attachment-shaped delivery would ever reach it.
    #[must_use]
    pub fn renders_outcome(&self, connection: ConnectionId, attached: &HashSet<SessionId>) -> bool {
        connection == self.requester || self.renders_request(connection, attached)
    }

    /// The session whose consent minted the grant, as recorded in the grant's
    /// key — see [`crate::grants::Grant`].
    ///
    /// For the attach arms it is the target session (or nothing, when the
    /// requester approved its own request and the target is already known). For
    /// a monitor request it is one of the approving surface's sessions: a
    /// monitor grant's key does not turn on it ([`crate::grants::may_monitor`]
    /// reads only the scope), but recording a real session keeps the entry
    /// honest about where the consent came from. The pick is the smallest id
    /// rather than "whichever the set yields first", so the recorded witness is
    /// reproducible instead of hash-order dependent.
    #[must_use]
    pub fn approved_at(&self, attached: &HashSet<SessionId>) -> Option<SessionId> {
        match &self.rendered_by {
            RenderedBy::ConnectionsAttachedTo(session) => Some(session.clone()),
            RenderedBy::TheRequesterItself => None,
            RenderedBy::AnyAttachedPeer => attached.iter().min().cloned(),
        }
    }
}

/// One in-flight consent request: where it was asked, and who is waiting.
struct Waiter {
    /// The routing decision, taken once when the request was raised. Read back
    /// by `attach/consent` to authorize the answer, so the surface that was
    /// asked is the surface that may reply.
    route: ConsentRoute,
    /// Resolved by [`PendingConsents::resolve`]; dropping it denies.
    tx: oneshot::Sender<ConsentOutcome>,
}

/// The registry of in-flight consent requests, keyed by request id.
///
/// Daemon-wide, like the permission registry it is shaped after and for the
/// same reason: the answer arrives on whichever connection the user happened to
/// be looking at, and it has to find the waiter regardless of which connection
/// raised it.
#[derive(Default)]
pub struct PendingConsents {
    waiters: Mutex<HashMap<RequestId, Waiter>>,
    // The counter lives HERE — beside the map that resolves the id — and
    // nowhere else (LESSON-503, BUG-161). A per-connection or per-session
    // counter would mint `consent-0` twice and let one request's answer mint
    // another request's grant, which on this seam is not a mis-route but a
    // cross-connection authorization.
    counter: AtomicU64,
}

impl PendingConsents {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint the next request id. Daemon-wide and monotonic, so no two consent
    /// requests ever share one.
    pub fn next_request_id(&self) -> RequestId {
        RequestId::from(format!(
            "consent-{}",
            self.counter.fetch_add(1, Ordering::SeqCst)
        ))
    }

    /// Register a waiter for `id` and return the receiver the requesting task
    /// awaits. `id` must come from [`Self::next_request_id`].
    ///
    /// Refuses to overwrite an existing waiter rather than replacing it. With
    /// ids minted above this is unreachable, so a collision here means a
    /// per-scope counter has crept back (the BUG-161 shape) — the first waiter
    /// is kept and this caller's receiver resolves to the safe default
    /// ([`ConsentOutcome::Denied`], via the dropped sender). Never a silent
    /// steal: the route recorded for a live request id can never be rewritten,
    /// so the set of surfaces entitled to answer a pending prompt is fixed the
    /// moment it is raised.
    pub fn register(
        &self,
        id: RequestId,
        route: ConsentRoute,
    ) -> oneshot::Receiver<ConsentOutcome> {
        let (tx, rx) = oneshot::channel();
        let mut waiters = self
            .waiters
            .lock()
            .expect("pending consents mutex poisoned");
        match waiters.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(Waiter { route, tx });
            }
            Entry::Occupied(existing) => {
                // The id is `consent-N`, never content — safe to log
                // (conventions: privacy in logs).
                eprintln!(
                    "tetond: attach consent request_id collision on {:?} — refusing to overwrite \
                     the waiting request (BUG-161 tripwire)",
                    existing.key()
                );
                // `tx` drops here → the caller's `rx` errors → its decision
                // takes the `Denied` arm.
            }
        }
        rx
    }

    /// The route a live request was raised on, or `None` if no request by that
    /// id is outstanding.
    ///
    /// A read only — it never consumes the waiter, because a caller about to be
    /// **refused** must leave the prompt standing for whoever may rightfully
    /// answer it. (The `owner_of`-before-`resolve` shape, for the same reason:
    /// otherwise a stranger could cancel any pending consent at will.)
    #[must_use]
    pub fn route_of(&self, id: &RequestId) -> Option<ConsentRoute> {
        self.waiters
            .lock()
            .expect("pending consents mutex poisoned")
            .get(id)
            .map(|waiter| waiter.route.clone())
    }

    /// Deliver a decision to the waiting request. Returns `true` if a waiter was
    /// present *and* still listening.
    ///
    /// Consumes the waiter, so it runs only after [`Self::route_of`] has
    /// authorized the answer. A `false` return is the honest answer to "did my
    /// decision decide anything": the window had closed, or another surface
    /// answered first.
    pub fn resolve(&self, id: &RequestId, outcome: ConsentOutcome) -> bool {
        let waiter = self
            .waiters
            .lock()
            .expect("pending consents mutex poisoned")
            .remove(id);
        match waiter {
            Some(waiter) => waiter.tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Drop a request's waiter without deciding it. Idempotent.
    ///
    /// BR-7's "leaves no partial state": a timed-out request must not sit in
    /// this map afterwards, where a late answer could still find it and resolve
    /// a decision whose requester has already been refused.
    pub fn forget(&self, id: &RequestId) -> bool {
        self.waiters
            .lock()
            .expect("pending consents mutex poisoned")
            .remove(id)
            .is_some()
    }

    /// Await the decision on `id`, defaulting closed after `window`.
    ///
    /// The three endings — answered, sender dropped, window elapsed — are
    /// folded here rather than at each call site so BR-7's guarantee is one
    /// piece of code: whichever way it ends, this registry keeps nothing.
    ///
    /// Only the timeout arm calls [`Self::forget`], and that is not an
    /// omission. An answered request was already taken by `resolve`, and a
    /// dropped sender means no waiter was ever stored under this id for *this*
    /// caller — the collision path, where forgetting would evict the live
    /// request that legitimately owns the id.
    pub async fn await_decision(
        &self,
        id: &RequestId,
        rx: oneshot::Receiver<ConsentOutcome>,
        window: Duration,
    ) -> ConsentOutcome {
        match tokio::time::timeout(window, rx).await {
            Ok(Ok(outcome)) => outcome,
            // The sender went without an answer — a collision-refused
            // registration, or a registry torn down. Denied is the only safe
            // reading of "nobody will ever answer this".
            Ok(Err(_)) => ConsentOutcome::Denied,
            Err(_elapsed) => {
                // BR-7: the window closing ends the request, so nothing may be
                // left behind for a late answer to find and decide.
                self.forget(id);
                ConsentOutcome::TimedOut
            }
        }
    }

    /// Number of requests currently awaiting a decision.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.waiters
            .lock()
            .expect("pending consents mutex poisoned")
            .len()
    }
}

/// One live connection, as far as consent routing is concerned.
struct Surface {
    /// The connection's attachment set — **the same** `Arc` its `ConnState`
    /// holds, never a copy. A copy would be a second answer to "what is this
    /// connection attached to", and the routing rule would start deciding
    /// against a stale one (LESSON-484: one definition, in one place).
    attached: Arc<RwLock<HashSet<SessionId>>>,
    /// The connection's outbound frame channel.
    out: mpsc::Sender<String>,
}

/// Every live connection a consent prompt could be rendered at.
///
/// The daemon otherwise keeps no registry of connections — each one is a task
/// with its own state — and this exists because BR-6's routing asks a question
/// no single connection can answer: *is anyone else attached to this session?*
/// Registered at the handshake and released when the connection ends, at the
/// same one place grants are released, so a surface can never outlive the
/// client behind it.
#[derive(Default)]
pub struct ConsentSurfaces {
    live: RwLock<HashMap<ConnectionId, Surface>>,
}

impl ConsentSurfaces {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a handshaked connection as a possible consent surface.
    pub fn register(
        &self,
        connection: ConnectionId,
        attached: Arc<RwLock<HashSet<SessionId>>>,
        out: mpsc::Sender<String>,
    ) {
        self.live
            .write()
            .expect("consent surfaces lock poisoned")
            .insert(connection, Surface { attached, out });
    }

    /// Forget a connection that has ended. Idempotent.
    pub fn release(&self, connection: ConnectionId) {
        self.live
            .write()
            .expect("consent surfaces lock poisoned")
            .remove(&connection);
    }

    /// Is any live connection attached to `session`?
    ///
    /// The question that picks BR-6's first arm over its second. Racy by
    /// nature — a client can attach or leave a microsecond later — and
    /// deliberately not locked against: the worst outcomes are a prompt offered
    /// to the requester while someone else was also entitled to answer it (both
    /// are legitimate surfaces), or a prompt offered to a peer that then leaves,
    /// which the bounded window already handles.
    #[must_use]
    pub fn anyone_attached_to(&self, session: &SessionId) -> bool {
        self.live
            .read()
            .expect("consent surfaces lock poisoned")
            .values()
            .any(|surface| {
                surface
                    .attached
                    .read()
                    .expect("connection attachment lock poisoned")
                    .contains(session)
            })
    }

    /// Is any live connection other than `connection` attached to *some*
    /// session?
    ///
    /// The monitor precondition: a whole-daemon read capability is approved
    /// only at a surface the user already owns, so if there is no such surface
    /// the request is refused rather than put to the requester itself.
    #[must_use]
    pub fn anyone_attached_besides(&self, connection: ConnectionId) -> bool {
        self.live
            .read()
            .expect("consent surfaces lock poisoned")
            .iter()
            .any(|(id, surface)| {
                *id != connection
                    && !surface
                        .attached
                        .read()
                        .expect("connection attachment lock poisoned")
                        .is_empty()
            })
    }

    /// Send `frame` to every surface this route offers the prompt to. Returns
    /// how many took it.
    pub fn deliver_request(&self, route: &ConsentRoute, frame: &str) -> usize {
        self.deliver(frame, |connection, attached| {
            route.renders_request(connection, attached)
        })
    }

    /// Send `frame` to every surface entitled to learn how the request ended.
    pub fn deliver_outcome(&self, route: &ConsentRoute, frame: &str) -> usize {
        self.deliver(frame, |connection, attached| {
            route.renders_outcome(connection, attached)
        })
    }

    /// The delivery mechanism behind both.
    ///
    /// `try_send`, never a blocking send: this runs while the caller holds the
    /// surface map, and a client too wedged to accept a frame must not be able
    /// to stall the connection that is asking about it — the event bus's
    /// posture, for the same reason. A dropped prompt costs the requester a
    /// timeout, which is the fail-closed direction (BR-7).
    fn deliver(
        &self,
        frame: &str,
        mut wanted: impl FnMut(ConnectionId, &HashSet<SessionId>) -> bool,
    ) -> usize {
        let live = self.live.read().expect("consent surfaces lock poisoned");
        let mut sent = 0;
        for (connection, surface) in live.iter() {
            let attached = surface
                .attached
                .read()
                .expect("connection attachment lock poisoned");
            if wanted(*connection, &attached) && surface.out.try_send(frame.to_owned()).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// How many connections are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
            .read()
            .expect("consent surfaces lock poisoned")
            .len()
    }

    /// Whether any connection is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::grants::GrantRegistry;

    fn session(name: &str) -> SessionId {
        SessionId::from(name)
    }

    fn attached_to(sessions: &[&str]) -> HashSet<SessionId> {
        sessions.iter().map(|s| session(s)).collect()
    }

    /// BR-6's routing rule as a table: every arm crossed with every kind of
    /// connection that could try to answer.
    ///
    /// The cells that matter are the *negative* ones. A connection attached to
    /// some other session must not be able to approve this session's request,
    /// and the requester must not be able to approve its own — except in the
    /// one arm where the daemon deliberately asked it to.
    #[test]
    fn a_route_offers_a_prompt_to_exactly_the_surfaces_it_names() {
        let registry = GrantRegistry::new();
        let requester = registry.next_connection_id();
        let peer = registry.next_connection_id();
        let target = session("sess-target");

        let arm1 = ConsentRoute::attached_to(requester, target.clone());
        let arm2 = ConsentRoute::requester_itself(requester);
        let monitor = ConsentRoute::any_attached_peer(requester);

        // (case, route, who, what they are attached to, may render/answer)
        let cases: Vec<(&str, &ConsentRoute, ConnectionId, HashSet<SessionId>, bool)> = vec![
            (
                "arm 1: a peer attached to the target is asked",
                &arm1,
                peer,
                attached_to(&["sess-target"]),
                true,
            ),
            (
                "arm 1: a peer attached to something else is not",
                &arm1,
                peer,
                attached_to(&["sess-other"]),
                false,
            ),
            (
                "arm 1: a peer attached to nothing is not",
                &arm1,
                peer,
                HashSet::new(),
                false,
            ),
            (
                "arm 1: the requester may not answer its own request",
                &arm1,
                requester,
                HashSet::new(),
                false,
            ),
            (
                "arm 2: the requester renders its own prompt",
                &arm2,
                requester,
                HashSet::new(),
                true,
            ),
            (
                "arm 2: and nobody else may answer it",
                &arm2,
                peer,
                attached_to(&["sess-target"]),
                false,
            ),
            (
                "monitor: any attached peer may answer",
                &monitor,
                peer,
                attached_to(&["sess-other"]),
                true,
            ),
            (
                "monitor: an unattached peer may not",
                &monitor,
                peer,
                HashSet::new(),
                false,
            ),
            (
                "monitor: the requester may never self-approve, attached or not",
                &monitor,
                requester,
                attached_to(&["sess-target"]),
                false,
            ),
        ];

        for (case, route, who, attached, expected) in cases {
            assert_eq!(
                route.renders_request(who, &attached),
                expected,
                "{case}: renders_request"
            );
        }
    }

    /// The requester always learns how its own request ended, even in the arms
    /// that never offered it the prompt — otherwise the one connection that has
    /// to retire its pending state is the one connection never told.
    #[test]
    fn the_requester_is_told_the_outcome_of_a_prompt_it_was_never_offered() {
        let registry = GrantRegistry::new();
        let requester = registry.next_connection_id();
        let stranger = registry.next_connection_id();
        let route = ConsentRoute::attached_to(requester, session("sess-target"));
        let nothing = HashSet::new();

        assert!(!route.renders_request(requester, &nothing));
        assert!(route.renders_outcome(requester, &nothing));
        assert!(
            !route.renders_outcome(stranger, &nothing),
            "an uninvolved connection learns nothing either way"
        );
    }

    /// REQ-569 TASK-109: which answers count as a **self-approval** — the arm
    /// where nobody but the requester was ever involved.
    ///
    /// The negative cells are the point. A grant approved by an attached peer
    /// is a decision two connections took part in and must never be logged as a
    /// self-approval, or the log line stops distinguishing the residual from
    /// the good case and becomes noise a reader learns to skip.
    #[test]
    fn only_the_requester_answering_its_own_rendered_prompt_is_a_self_approval() {
        let registry = GrantRegistry::new();
        let requester = registry.next_connection_id();
        let peer = registry.next_connection_id();
        let target = session("sess-target");

        assert!(
            ConsentRoute::requester_itself(requester).self_approved_by(requester),
            "the resume arm answered by the connection that asked IS the residual"
        );
        assert!(
            !ConsentRoute::requester_itself(requester).self_approved_by(peer),
            "a different connection cannot self-approve somebody else's request"
        );
        assert!(
            !ConsentRoute::attached_to(requester, target.clone()).self_approved_by(requester),
            "the peer arm is a second party's decision, whoever answers it"
        );
        assert!(
            !ConsentRoute::attached_to(requester, target).self_approved_by(peer),
            "an attached peer approving is the good case, not the residual"
        );
        assert!(
            !ConsentRoute::any_attached_peer(requester).self_approved_by(requester),
            "monitor has no self-render arm at all"
        );
    }

    /// The session recorded in the minted grant's key, per arm.
    #[test]
    fn the_witness_session_is_the_target_for_attach_and_the_approvers_for_monitor() {
        let registry = GrantRegistry::new();
        let requester = registry.next_connection_id();
        let target = session("sess-target");

        assert_eq!(
            ConsentRoute::attached_to(requester, target.clone()).approved_at(&HashSet::new()),
            Some(target.clone())
        );
        assert_eq!(
            ConsentRoute::requester_itself(requester).approved_at(&attached_to(&["sess-a"])),
            None
        );
        // Deterministic across runs, not hash-order dependent.
        let monitor = ConsentRoute::any_attached_peer(requester);
        let approver = attached_to(&["sess-b", "sess-a", "sess-c"]);
        assert_eq!(monitor.approved_at(&approver), Some(session("sess-a")));
        assert_eq!(monitor.approved_at(&HashSet::new()), None);
    }

    /// LESSON-503: ids come from the map that resolves them, so no two requests
    /// — on any connection, for any scope — can share one.
    #[test]
    fn consent_ids_are_minted_daemon_wide_and_never_repeat() {
        let pending = PendingConsents::new();
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(pending.next_request_id()),
                "a consent request id was handed out twice"
            );
        }
    }

    /// The BUG-161 tripwire: a second `register` on a live id keeps the first
    /// waiter and denies the second, rather than silently stealing the prompt.
    ///
    /// Unreachable through `next_request_id`, which is exactly why it is tested
    /// directly — the guard exists so that a future per-scope counter fails
    /// loudly instead of re-creating a cross-request authorization.
    #[tokio::test]
    async fn a_colliding_registration_cannot_steal_a_live_consent_request() {
        let registry = GrantRegistry::new();
        let first_requester = registry.next_connection_id();
        let second_requester = registry.next_connection_id();
        let pending = PendingConsents::new();
        let id = pending.next_request_id();

        let first = pending.register(id.clone(), ConsentRoute::requester_itself(first_requester));
        let second = pending.register(
            id.clone(),
            ConsentRoute::any_attached_peer(second_requester),
        );

        assert_eq!(
            pending.pending_count(),
            1,
            "the collision must not add a second waiter"
        );
        assert_eq!(
            pending.route_of(&id),
            Some(ConsentRoute::requester_itself(first_requester)),
            "the route of a live request must not be rewritten by a later registration"
        );

        // The displaced caller is denied by its dropped sender rather than left
        // hanging or handed the other request's answer.
        assert_eq!(
            pending
                .await_decision(&id, second, Duration::from_secs(5))
                .await,
            ConsentOutcome::Denied
        );

        // And the first waiter is still the one an answer reaches.
        assert!(pending.resolve(&id, ConsentOutcome::Granted { approved_at: None }));
        assert_eq!(
            first.await,
            Ok(ConsentOutcome::Granted { approved_at: None })
        );
        assert_eq!(pending.pending_count(), 0);
    }

    /// BR-7: every ending leaves the registry empty — asserted for each of the
    /// three, because "no partial state" is a claim about the ending you did
    /// not think about.
    #[tokio::test]
    async fn every_ending_leaves_no_residual_entry() {
        let registry = GrantRegistry::new();
        let requester = registry.next_connection_id();
        let pending = PendingConsents::new();

        // Answered.
        let id = pending.next_request_id();
        let rx = pending.register(id.clone(), ConsentRoute::requester_itself(requester));
        assert!(pending.resolve(&id, ConsentOutcome::Denied));
        assert_eq!(
            pending
                .await_decision(&id, rx, Duration::from_secs(5))
                .await,
            ConsentOutcome::Denied
        );
        assert_eq!(pending.pending_count(), 0);

        // Timed out.
        let id = pending.next_request_id();
        let rx = pending.register(id.clone(), ConsentRoute::requester_itself(requester));
        assert_eq!(pending.pending_count(), 1);
        assert_eq!(
            pending
                .await_decision(&id, rx, Duration::from_millis(20))
                .await,
            ConsentOutcome::TimedOut
        );
        assert_eq!(
            pending.pending_count(),
            0,
            "a timed-out request must not sit in the map for a late answer to find"
        );
        assert!(
            !pending.resolve(&id, ConsentOutcome::Granted { approved_at: None }),
            "and a late answer must find nothing to resolve"
        );

        // Forgotten (the requester's connection ended).
        let id = pending.next_request_id();
        let _rx = pending.register(id.clone(), ConsentRoute::requester_itself(requester));
        assert!(pending.forget(&id));
        assert!(!pending.forget(&id), "forget is idempotent");
        assert_eq!(pending.pending_count(), 0);
    }

    /// An answer to an id nobody is waiting on decides nothing — and says so.
    #[test]
    fn an_unknown_request_id_resolves_nothing() {
        let pending = PendingConsents::new();
        assert!(pending.route_of(&RequestId::from("consent-99")).is_none());
        assert!(!pending.resolve(&RequestId::from("consent-99"), ConsentOutcome::Denied));
    }

    /// The surface registry answers the two questions BR-6's routing turns on,
    /// and stops answering them the moment a connection ends.
    #[test]
    fn surfaces_report_who_is_attached_and_forget_a_connection_that_ended() {
        let registry = GrantRegistry::new();
        let surfaces = ConsentSurfaces::new();
        let holder = registry.next_connection_id();
        let idle = registry.next_connection_id();
        let target = session("sess-target");

        let holder_attached = Arc::new(RwLock::new(HashSet::new()));
        let (holder_tx, _holder_rx) = mpsc::channel(8);
        surfaces.register(holder, Arc::clone(&holder_attached), holder_tx);
        let (idle_tx, _idle_rx) = mpsc::channel(8);
        surfaces.register(idle, Arc::new(RwLock::new(HashSet::new())), idle_tx);

        assert!(!surfaces.anyone_attached_to(&target));
        assert!(!surfaces.anyone_attached_besides(idle));

        // The registry shares the *same* set the connection mutates, so an
        // attach that happens elsewhere is visible here without anyone
        // re-registering anything.
        holder_attached.write().unwrap().insert(target.clone());
        assert!(surfaces.anyone_attached_to(&target));
        assert!(surfaces.anyone_attached_besides(idle));
        assert!(
            !surfaces.anyone_attached_besides(holder),
            "the only attached connection cannot be its own approver"
        );

        surfaces.release(holder);
        assert!(
            !surfaces.anyone_attached_to(&target),
            "a departed connection is not a surface anyone can be asked at"
        );
        assert_eq!(surfaces.len(), 1);
    }

    /// Delivery follows the route, and only the route.
    #[tokio::test]
    async fn a_prompt_reaches_the_routed_surface_and_no_other() {
        let registry = GrantRegistry::new();
        let surfaces = ConsentSurfaces::new();
        let requester = registry.next_connection_id();
        let holder = registry.next_connection_id();
        let bystander = registry.next_connection_id();
        let target = session("sess-target");

        let (requester_tx, mut requester_rx) = mpsc::channel(8);
        surfaces.register(
            requester,
            Arc::new(RwLock::new(HashSet::new())),
            requester_tx,
        );
        let holder_attached = Arc::new(RwLock::new([target.clone()].into_iter().collect()));
        let (holder_tx, mut holder_rx) = mpsc::channel(8);
        surfaces.register(holder, holder_attached, holder_tx);
        let (bystander_tx, mut bystander_rx) = mpsc::channel(8);
        surfaces.register(
            bystander,
            Arc::new(RwLock::new([session("sess-other")].into_iter().collect())),
            bystander_tx,
        );

        let route = ConsentRoute::attached_to(requester, target);
        assert_eq!(surfaces.deliver_request(&route, "prompt"), 1);
        assert_eq!(holder_rx.try_recv().ok(), Some("prompt".to_owned()));
        assert!(requester_rx.try_recv().is_err());
        assert!(bystander_rx.try_recv().is_err());

        // The outcome goes one wider: the requester is always told.
        assert_eq!(surfaces.deliver_outcome(&route, "refused"), 2);
        assert_eq!(holder_rx.try_recv().ok(), Some("refused".to_owned()));
        assert_eq!(requester_rx.try_recv().ok(), Some("refused".to_owned()));
        assert!(bystander_rx.try_recv().is_err());
    }
}
