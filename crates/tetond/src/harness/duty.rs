//! The **shared duty seam**: one resolved route, one trait, one local impl, one
//! remote impl, one egress scoping call, one ceiling (REQ-561 BR-6, ADR-1/ADR-2).
//!
//! ## What a "duty" is
//!
//! A duty is a model call the *harness* makes on its own behalf, as opposed to
//! the turn the user asked for. `digest` is the first one: `summarize_if_large`
//! condenses an oversized tool result before it enters context. It is one
//! instruction in, one bounded string out — never a conversation, never a tool
//! call, never a lifecycle position.
//!
//! ## Why one seam rather than one per category
//!
//! REQ-558 built `digest` as a one-off — a route enum, a trait, a local/remote
//! pair, an egress call, a ceiling constant — roughly 260 lines of machinery for
//! one category. REQ-561 adds four more callers, and copying that shape four
//! times would produce five parallel implementations of the same five concerns.
//! So [`DutyRoute`] is **one non-generic type** holding an [`Arc<dyn Duty>`]
//! (ADR-1): a generic `DutyRoute<T>` monomorphises into five distinct types,
//! which is the same outcome expressed in the type system instead of in copied
//! code.
//!
//! What stays per-category is bounded to three things, and none of them are
//! here: the one-line resolver in [`crate::runtime`] that names the category
//! literally (ADR-3 — the `declared, no call site yet` marker in
//! [`crate::call_sites`] is derived by scanning for exactly that spelling, so
//! collapsing it into a helper taking a category *variable* would make the scan
//! blind), the duty's output-contract constant, and its prompt builder.
//!
//! ## Two implementations, one seam (mirroring `completion.rs`)
//!
//! - [`LocalDuty`] holds an [`Engine`] and **no transport**, so egress is
//!   impossible on that path by construction — exactly
//!   [`LocalEngineSource`](super::completion::LocalEngineSource)'s posture.
//! - [`RemoteDuty`] reaches the network **only** through the provenance-scoped
//!   `&dyn Transport` that [`Egress::scoped`] produces, so a duty over a
//!   `local-only` file is refused before a byte leaves and is billed as one
//!   `CostRecord` against the duty's own category (BR-1/BR-2).
//!
//! ## The provenance is the caller's (ADR-2)
//!
//! [`Duty::perform`] takes the already-merged egress [`Provenance`] of *the
//! content it is about to send* — not a tool-shaped wrapper it would have to
//! know how to interpret. Each call site computes it from the content it is
//! handing over ([`digest`](super::digest::tool_result_provenance) converts a
//! [`ToolProvenance`](super::context::ToolProvenance)), which makes BR-7's
//! "scoped by the content it sends" a property of the signature rather than a
//! convention every duty has to remember.
//!
//! ## `route_decided` fires on **performance**, not on resolution (BR-2)
//!
//! BR-2 exists to make a new *egress path* visible. A path that never fires
//! produced no egress, so announcing a routing decision for a duty that never
//! runs would be observing a resolution, not an egress. And the arithmetic is
//! decisive: `digest_route()` is built unconditionally once per turn attempt
//! whether or not any tool result crosses the threshold, so emitting at
//! resolution would put five spurious `route_decided` events on every turn once
//! all five duties are wired.
//!
//! So [`DutyRoute::announcing`] *attaches* the payload the resolver projected,
//! and [`DutyRoute::perform`] publishes it — once per invocation, because each
//! invocation is a real routed model call, which is exactly what `route_decided`
//! means. The publish sits in the seam rather than at each call site on purpose:
//! the seam is the one place all five duties share, so one emission site here is
//! BR-6 working, not a concession.
//!
//! ## Failure is never silence (LESSON-447)
//!
//! A duty cannot decline. An implementation that cannot serve returns `Err`
//! carrying a broadcast-safe sentence, and an unresolvable binding is a
//! [`DutyRoute::Unresolved`] carrying the resolver's own reason. Both leave the
//! call site holding an explanation it can degrade with, which is the whole
//! reason neither is a bare `None`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use teton_inference::{Engine, GenParams};
use teton_protocol::events::{Event, RouteDecided};
use teton_protocol::{Category, ProviderId, SessionId};
use teton_providers::{Message, Provider, Role, Transport, TurnEvent, TurnRequest};

use crate::broadcast::EventBus;
use crate::cost::CostAttribution;
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::floor_char_boundary;
use super::render::render_duty;

/// What a duty *is*, independent of where it runs: the two facts every
/// implementation needs and neither of which depends on the tier.
///
/// Carried as one value rather than two parameters because they always travel
/// together and are always stated together — one `const` per category, beside
/// that category's output contract, is the entire per-category surface ADR-3
/// allows. The route builders take it and the [`Duty`] impls project it back
/// out through [`Duty::category`] and [`Duty::ceiling_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DutyKind {
    category: Category,
    ceiling_bytes: usize,
}

impl DutyKind {
    /// A duty of `category` whose output is bounded to `ceiling_bytes` (BR-8).
    #[must_use]
    pub const fn new(category: Category, ceiling_bytes: usize) -> Self {
        Self {
            category,
            ceiling_bytes,
        }
    }

    /// The category this duty attributes its spend to and routes through.
    #[must_use]
    pub const fn category(self) -> Category {
        self.category
    }

    /// The harness-owned byte ceiling on what this duty may return.
    #[must_use]
    pub const fn ceiling_bytes(self) -> usize {
        self.ceiling_bytes
    }
}

/// Something that can serve one duty call (ADR-2).
///
/// `Send + Sync` because the turn loop holds the route across awaits and drives
/// it from any task. The trait is deliberately **narrow**: one prompt in, one
/// bounded string or one error message out. Every duty returns text — a
/// structured decision is parsed *at the call site*, never inside the seam,
/// because a parser here would make the seam aware of the category it is meant
/// to be indifferent to.
#[async_trait]
pub trait Duty: Send + Sync {
    /// The duty's own category — used for cost attribution and `route_decided`.
    fn category(&self) -> Category;

    /// The harness-owned output ceiling (BR-8), enforced by the implementation
    /// rather than requested of the provider (LESSON-484).
    fn ceiling_bytes(&self) -> usize;

    /// Perform the duty over `prompt`, whose embedded content came from
    /// `provenance`.
    ///
    /// `provenance` is the egress provenance of the *content being sent*, not of
    /// the conversation. A local implementation ignores it and has no transport
    /// to use it with; a remote one MUST scope its egress by it.
    ///
    /// # Errors
    /// Returns the failure as a broadcast-safe sentence — an engine error, a
    /// provider error, or a refusal at the choke point. Never the model's own
    /// output and never the content.
    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String>;
}

/// The `route_decided` a duty publishes when it actually runs (BR-2).
///
/// Holds the payload the resolver already projected off its `Route` rather than
/// the `Route` itself: the seam has no business knowing about routing types, and
/// projecting the event twice is the drift ADR-D exists to prevent.
///
/// Public only because it rides a public enum variant's field. Its own fields
/// are private and it has no constructor, so the only way to attach one is
/// [`DutyRoute::announcing`] and the only thing that can fire one is
/// [`DutyRoute::perform`] — nothing outside this module can announce a duty
/// route that no duty ran.
pub struct DutyAnnouncement {
    bus: Arc<EventBus>,
    session_id: Option<SessionId>,
    decided: RouteDecided,
}

impl DutyAnnouncement {
    fn publish(&self) {
        self.bus.publish(
            self.session_id.clone(),
            Event::RouteDecided(self.decided.clone()),
        );
    }
}

// ── A trap the next four duty tasks will otherwise rediscover ──────────────
//
// `call_sites.rs`'s derived-marker test scans the daemon's production source as
// **text** — doc comments and ordinary comments included, before anything is
// compiled — looking for a router-resolution call with a `Category::` literal
// inside its parentheses. Spelling that call out in prose anywhere in `src/`,
// even as an illustration and even with a placeholder variant name, makes the
// scan read the prose as a real call site, fail to match the placeholder
// against any category, and turn red with "the scan cannot tell which category
// this dispatches on". Describe the resolver in words; never write its
// spelling. (Cost of learning this the hard way: one confusing red test in a
// file that had not yet been compiled into the crate.)

/// One duty category, resolved for this turn.
///
/// Two states, and the second is not an error to be swallowed: an unresolvable
/// binding is a **routing failure with a reason**, and the reason is the
/// resolver's own sentence (BR-6 — this type mints no explanation of its own).
/// The call site reports it and still enforces its invariant by degraded means.
pub enum DutyRoute {
    /// Resolved: `provider_id` serves the duty through `duty`.
    Serves {
        /// The provider the duty's category resolved to. Carried so the failure
        /// log — and a test — can say *where* a duty went, rather than only that
        /// one happened.
        provider_id: String,
        /// What performs the call. One `dyn` object, not a type parameter
        /// (ADR-1): a duty performs a model call, so one vtable indirection is
        /// free next to the inference.
        duty: Arc<dyn Duty>,
        /// What to announce when this duty actually runs, or `None` for a route
        /// no router decided — the transport-free offline entry point builds one
        /// of those, and a path with no routing decision has nothing to report.
        announce: Option<DutyAnnouncement>,
    },
    /// Unresolved: nothing can serve this duty for this turn.
    Unresolved {
        /// The resolver's sentence, verbatim.
        reason: String,
    },
}

impl DutyRoute {
    /// A route served by the local tier's `engine`.
    #[must_use]
    pub fn local(
        kind: DutyKind,
        provider_id: impl Into<String>,
        engine: Arc<Mutex<dyn Engine>>,
    ) -> Self {
        DutyRoute::Serves {
            provider_id: provider_id.into(),
            duty: Arc::new(LocalDuty { kind, engine }),
            announce: None,
        }
    }

    /// A route served by a remote `provider`, reaching the network only through
    /// `egress`.
    #[must_use]
    pub fn remote<T: Transport + 'static>(
        kind: DutyKind,
        provider_id: impl Into<String>,
        provider: Box<dyn Provider>,
        egress: Egress<T>,
        model: impl Into<String>,
        session_id: impl Into<SessionId>,
    ) -> Self {
        let provider_id = provider_id.into();
        DutyRoute::Serves {
            duty: Arc::new(RemoteDuty {
                kind,
                provider,
                egress,
                provider_id: ProviderId::from(provider_id.clone()),
                model: model.into(),
                session_id: session_id.into(),
            }),
            provider_id,
            announce: None,
        }
    }

    /// A route that resolved to nothing, explained by `reason`.
    #[must_use]
    pub fn unresolved(reason: impl Into<String>) -> Self {
        DutyRoute::Unresolved {
            reason: reason.into(),
        }
    }

    /// Attach the `route_decided` this route will announce **if and when** the
    /// duty actually runs (BR-2).
    ///
    /// `decided` is `Option` for the same reason
    /// [`Router::emit_route_decided`](crate::router::Router::emit_route_decided)
    /// self-guards: a resolution that selected no provider projects no event, so
    /// a duty that could not be routed announces nothing rather than announcing
    /// a decision that was never made. A no-op on an [`Unresolved`] route, which
    /// can never reach [`Self::perform`]'s publishing arm anyway.
    ///
    /// [`Unresolved`]: DutyRoute::Unresolved
    #[must_use]
    pub fn announcing(
        self,
        bus: &Arc<EventBus>,
        session_id: Option<SessionId>,
        decided: Option<RouteDecided>,
    ) -> Self {
        match (self, decided) {
            (
                DutyRoute::Serves {
                    provider_id, duty, ..
                },
                Some(decided),
            ) => DutyRoute::Serves {
                provider_id,
                duty,
                announce: Some(DutyAnnouncement {
                    bus: Arc::clone(bus),
                    session_id,
                    decided,
                }),
            },
            (route, _) => route,
        }
    }

    /// Run the duty, announcing its route first (BR-2).
    ///
    /// The announcement is published **before** the call rather than after it,
    /// so the ordering a client observes matches the turn path's — the route,
    /// then whatever that route produced (a `privacy_block`, a `cost` record).
    /// It fires once per invocation and is not deduplicated: two oversized tool
    /// results are two routed model calls, and collapsing them would under-report
    /// egress on exactly the turns that egress most.
    ///
    /// # Errors
    /// The duty's own failure sentence, or — for a route that resolved to
    /// nothing — the resolver's reason. Callers normally take the
    /// [`Unresolved`](DutyRoute::Unresolved) arm before reaching here, so that
    /// case is a belt rather than a path.
    pub async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String> {
        match self {
            DutyRoute::Serves { duty, announce, .. } => {
                if let Some(announce) = announce {
                    announce.publish();
                }
                duty.perform(prompt, provenance).await
            }
            DutyRoute::Unresolved { reason } => Err(reason.clone()),
        }
    }

    /// The provider serving this duty this turn, or `None` when it is
    /// unresolved.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match self {
            DutyRoute::Serves { provider_id, .. } => Some(provider_id),
            DutyRoute::Unresolved { .. } => None,
        }
    }
}

/// The local tier serving a duty: an [`Engine`] handle and **no transport**.
///
/// The absence is the guarantee, not an omission — this struct has no field a
/// network call could be made through, which is why [`Duty::perform`]'s
/// `provenance` argument is ignorable here without a boundary check. Adding one
/// would be a guard placed where it is convenient rather than where the decision
/// is made (LESSON-484); the decision is "which route", and it was already made.
struct LocalDuty {
    kind: DutyKind,
    engine: Arc<Mutex<dyn Engine>>,
}

#[async_trait]
impl Duty for LocalDuty {
    fn category(&self) -> Category {
        self.kind.category()
    }

    fn ceiling_bytes(&self) -> usize {
        self.kind.ceiling_bytes()
    }

    async fn perform(&self, prompt: &str, _provenance: &Provenance) -> Result<String, String> {
        let engine = Arc::clone(&self.engine);
        let prompt = prompt.to_owned();
        // The completion runs on the blocking pool (E-3): with a real llama.cpp
        // engine a duty takes seconds, and this is awaited from the async turn
        // loop, where running it inline would park the tokio worker and stall
        // every other session's RPCs.
        let result = tokio::task::spawn_blocking(move || {
            let params = GenParams::default();
            let guard = engine.lock().expect("engine mutex poisoned");
            // REQ-554 BR-7: a duty prompt gets the same template treatment an
            // agent turn does. The format is read from the guard already held
            // here, inside the blocking task: taking a second lock on the async
            // path to ask the engine its format would park a tokio worker behind
            // whatever completion currently owns the mutex (LESSON-448).
            let format = guard.chat_format();
            let rendered = render_duty(format, &prompt);
            guard
                .complete(&rendered, &params, &mut |_| true)
                .map(|completion| completion.text)
        })
        .await;
        match result {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("the local summarization task did not complete".to_owned()),
        }
    }
}

/// A remote provider serving a duty, through the single egress choke point.
///
/// The provider is handed only the provenance-scoped `&dyn Transport` that
/// [`Egress::scoped`] produces, so it cannot reach the network any other way: a
/// duty over boundary-protected content is refused before a byte leaves (BR-1),
/// and an allowed one is metered into a `CostRecord` attributed to the duty's
/// own category (BR-2) — so a user who binds a tier remotely can see what their
/// harness duties are costing.
struct RemoteDuty<T: Transport> {
    kind: DutyKind,
    provider: Box<dyn Provider>,
    egress: Egress<T>,
    provider_id: ProviderId,
    model: String,
    session_id: SessionId,
}

#[async_trait]
impl<T: Transport> Duty for RemoteDuty<T> {
    fn category(&self) -> Category {
        self.kind.category()
    }

    fn ceiling_bytes(&self) -> usize {
        self.kind.ceiling_bytes()
    }

    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String> {
        let request = TurnRequest {
            model: self.model.clone(),
            // A duty is one instruction, not a conversation: no system prompt to
            // inherit and — crucially — **no tools**. A duty must not be able to
            // emit a tool call that the loop would then have to decide what to
            // do with.
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: prompt.to_owned(),
            }],
            tools: Vec::new(),
            max_tokens: GenParams::default().max_tokens,
        };
        // BR-2: a duty has no lifecycle position, so it attributes no phase — but
        // it does attribute its category, which is the whole point of routing it.
        let attribution = CostAttribution::new(self.model.clone()).with_category(self.category());
        let ctx = EgressContext::new(self.provider_id.clone())
            .with_session(self.session_id.clone())
            .with_cost(attribution);
        // BR-1: the provenance of the *content being sent*, computed by the call
        // site from what it is handing over — narrower than the turn's context,
        // and the reason a `local-only` read is refused while the rest of the
        // conversation still goes. This is the one `Egress::scoped` on the duty
        // path.
        let transport = self.egress.scoped(provenance.clone(), ctx);

        let ceiling = self.ceiling_bytes();
        let mut stream = self
            .provider
            .stream_turn(request, &transport)
            .await
            .map_err(|err| err.to_string())?;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|err| err.to_string())? {
                // Bounded on the way in, at the ONE ceiling-enforcement site on
                // the duty path. `max_tokens` is a request, and a request is not
                // a guarantee: a provider that ignores it — or a stream that
                // simply does not stop — would otherwise grow this buffer
                // without limit, on paths whose entire purpose is to SHRINK
                // their input. Past the ceiling, stop accumulating and let the
                // stream drain.
                TurnEvent::TextDelta(delta) => {
                    if text.len() < ceiling {
                        text.push_str(&delta);
                    }
                }
                // A duty was offered no tools, so a tool call is a provider
                // ignoring the request; drop it rather than fold it into the
                // answer. `Completed` carries usage, which the meter reads off
                // the stream at the choke point.
                TurnEvent::ToolCall(_) | TurnEvent::Completed(_) => {}
            }
        }
        // The last delta may straddle the bound, so trim to it here rather than
        // refusing a partial delta above — dropping a whole delta would cut the
        // answer at an arbitrary earlier point for no gain.
        if text.len() > ceiling {
            text.truncate(floor_char_boundary(&text, ceiling));
        }
        Ok(text.trim().to_owned())
    }
}
