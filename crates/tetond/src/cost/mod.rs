//! The cost ledger (BR-2): a CostRecord for every completed remote call.
//!
//! REQ-544's second differentiator, the live cost meter, rests on one rule:
//! **every remote model call produces exactly one [`CostRecord`], attributed to
//! `(session, phase, provider, model)`, and the meter is derived *only* from
//! those records** — no estimated or unattributed spend is ever shown as
//! actual. This module owns the recording, the pricing, and the aggregation
//! that back that promise.
//!
//! ## Where recording happens (the egress seam)
//!
//! Recording is wired at the single egress choke point (architecture D-2), so it
//! cannot be forgotten by any adapter. [`Egress::send`](crate::egress::Egress::send)
//! calls a [`CostMeter`] at the *allowed-forward* point: the meter wraps the
//! streaming response so that, when the stream completes, the turn's token usage
//! is read from it, priced, written to the append-only ledger, and broadcast as
//! a `cost_recorded` event. A blocked call never reaches the meter, so a
//! privacy-blocked turn is never billed; a retry flows through egress again and
//! is therefore recorded as its own call (BR-2: "retries recorded individually").
//!
//! ## Privacy (BR-7)
//!
//! A ledger row holds token counts and metadata **only** — session id, phase,
//! provider id, model name, input/output token counts, and computed cost. No
//! prompt text, no tool arguments, no credential, ever. The schema has no column
//! that could carry content; see [`ledger`].
//!
//! ## Web lookups (REQ-563 BR-7)
//!
//! The same store also holds one [`WebLookupRow`] per web lookup, in the sibling
//! `web_lookups` table (architecture D-7) — every lookup, including the free and
//! the refused ones, so `/cost` can say what a session reached out to. The same
//! privacy rule applies and binds harder there: a lookup row names the
//! destination **host** and never a full URL, a search query, or a key.
//!
//! ## Module map
//! - [`ledger`] — the append-only SQLite store, the [`CostMeter`] implementation,
//!   and the streamed-usage extractor.
//! - [`prices`] — the versioned TOML price table; unknown models are *unpriced*,
//!   never guessed (BR-2).
//! - [`report`] — per-session / per-phase / per-provider aggregation and the
//!   AC-4 savings-vs-frontier estimate (OQ-6), each labeled as an estimate.

pub mod ledger;
pub mod prices;
pub mod report;

use teton_protocol::events::{CostRecord, CostRecorded, Event};
use teton_protocol::{Category, Phase, ProviderId, SessionId};
use teton_providers::transport::TransportResponse;

use crate::broadcast::EventBus;

pub use ledger::{CostLedger, LedgerError, LedgerRow, WebLookupRow, WebOverrideRow};
pub use prices::{ModelPrice, PriceTable};
pub use report::{CostReport, GroupTotals, SavingsEstimate, UnpricedTotals, WebTotals};

/// The billing attribution a caller pins to a remote call *at call time*.
///
/// The egress choke point already knows the session and provider; this carries
/// the two things it does not: the lifecycle `phase` in effect (AC-4 requires
/// per-phase attribution to match the session's phase *at the moment of the
/// call*) and the concrete `model` billed. A caller attaches it with
/// [`EgressContext::with_cost`](crate::egress::EgressContext::with_cost); absent
/// it, egress forwards the call unmetered (e.g. a non-billable probe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostAttribution {
    /// Lifecycle phase in effect at call time; `None` in freeform mode.
    ///
    /// Retained after REQ-558 moved dispatch onto `category` (BR-11): the phase
    /// is what the spend is *attributed* to, the category is what it was *for*,
    /// and a freeform session has the second without the first.
    pub phase: Option<Phase>,
    /// Routing category the call was made for (REQ-558); `None` when the call
    /// was not routed through the category chain.
    pub category: Option<Category>,
    /// Concrete model the call bills (drives the price-table lookup).
    pub model: String,
    /// Whether this call is a **connection test** rather than a turn (REQ-581
    /// BR-5).
    ///
    /// A probe is billed exactly like a turn — same egress path, same price
    /// table, one ordinary ledger row — and this flag only lets the meter
    /// *count* it apart, so `teton cost` can say "1 probe" rather than show a
    /// user a call they asked no question for as though it were a turn. It is
    /// therefore a flag on the attribution and not a second recording path.
    ///
    /// `false` for every turn, which is the default [`CostAttribution::new`]
    /// gives: only the connection test opts in, via
    /// [`CostAttribution::probe`].
    pub probe: bool,
}

impl CostAttribution {
    /// Attribution for `model` with no structured phase (freeform mode) and no
    /// category.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            phase: None,
            category: None,
            model: model.into(),
            probe: false,
        }
    }

    /// Attribution for `model` in structured-mode `phase`.
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Attribution for `model` under routing `category`.
    ///
    /// The caller passes the category the routing decision *resolved*, never one
    /// derived a second time from the phase (REQ-558 ADR-D, BR-6).
    #[must_use]
    pub fn with_category(mut self, category: Category) -> Self {
        self.category = Some(category);
        self
    }

    /// Mark this call a connection test (REQ-581 BR-5).
    ///
    /// Changes nothing about how the call is sent or priced — the row is
    /// written, costed and broadcast exactly as a turn's is. It changes only
    /// what the row *says it was*, so the report can count probes apart.
    #[must_use]
    pub fn probe(mut self) -> Self {
        self.probe = true;
        self
    }
}

/// The seam egress calls to bill an allowed forward.
///
/// Defined here (not in [`crate::egress`]) so the choke point depends only on
/// this tiny trait, never on the SQLite ledger behind it — the same inversion
/// the privacy [`PrivacyEventSink`](crate::egress::PrivacyEventSink) uses. The
/// implementor ([`CostLedger`]) wraps `response` so the turn's streamed usage is
/// recorded when the body drains; a meter that cannot attribute the call (no
/// session scope) returns the response untouched.
pub trait CostMeter: Send + Sync {
    /// Wrap `response` so that, on stream completion, the call is priced and
    /// recorded against `session_id` / `provider_id` / `attribution`. Returns
    /// the (possibly wrapped) response; the byte stream is passed through
    /// unchanged so the adapter still parses the real body.
    fn meter_response(
        &self,
        response: TransportResponse,
        session_id: Option<SessionId>,
        provider_id: ProviderId,
        attribution: CostAttribution,
    ) -> TransportResponse;
}

/// A sink for **local-tier** usage rows (REQ-564 BR-9).
///
/// The remote tier records itself at the egress choke point, which every remote
/// call already flows through. The local tier flows through no such seam — it is
/// transport-free by construction — so it needs its own narrow sink, abstracted
/// for the same reasons [`CostEventSink`] is: the harness must not depend on the
/// concrete ledger, and tests need to capture what was recorded.
pub trait LocalUsageMeter: Send + Sync {
    /// Record one completed local call.
    ///
    /// `cached_tokens` is a component of `input_tokens`, not a substitute for
    /// part of it. Best-effort by contract: a ledger failure must never fail a
    /// turn, so this returns nothing.
    fn local_call(
        &self,
        session_id: &SessionId,
        attribution: &CostAttribution,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    );
}

/// A sink for the `cost_recorded` event emitted as each row is written.
///
/// Abstracted so the ledger does not depend on the concrete daemon event bus
/// (and so tests can capture emitted records), mirroring
/// [`PrivacyEventSink`](crate::egress::PrivacyEventSink). The daemon wires its
/// [`EventBus`]; a [`NoopCostSink`] drops events where none are needed.
pub trait CostEventSink: Send + Sync {
    /// Publish a `cost_recorded` event for a freshly written record.
    fn cost_recorded(&self, record: CostRecord);
}

/// The production sink: broadcast to attached clients over the daemon event bus,
/// scoped to the record's session.
impl CostEventSink for EventBus {
    fn cost_recorded(&self, record: CostRecord) {
        let session_id = Some(record.session_id.clone());
        self.publish(session_id, Event::CostRecorded(CostRecorded { record }));
    }
}

/// A sink that drops `cost_recorded` events — for contexts with no subscribers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCostSink;

impl CostEventSink for NoopCostSink {
    fn cost_recorded(&self, _record: CostRecord) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_builder_sets_phase_category_and_model() {
        let attr = CostAttribution::new("claude-opus-4")
            .with_phase(Phase::Review)
            .with_category(Category::Review);
        assert_eq!(attr.model, "claude-opus-4");
        assert_eq!(attr.phase, Some(Phase::Review));
        assert_eq!(attr.category, Some(Category::Review));

        // BR-11: the two are independent. A freeform turn has a category and no
        // phase, which is exactly why adding the first did not replace the
        // second.
        let freeform = CostAttribution::new("deepseek-chat").with_category(Category::Design);
        assert_eq!(freeform.phase, None);
        assert_eq!(freeform.category, Some(Category::Design));

        let unrouted = CostAttribution::new("deepseek-chat");
        assert_eq!(unrouted.phase, None);
        assert_eq!(unrouted.category, None);
    }

    /// REQ-581 BR-5: every turn is attributed as a turn, and only the
    /// connection test opts into the probe flag. The default matters as much as
    /// the builder — a flag that defaulted to `true` anywhere would count real
    /// spend as a test.
    #[test]
    fn only_a_connection_test_is_attributed_as_a_probe() {
        assert!(
            !CostAttribution::new("claude-fable-5").probe,
            "a turn is never a probe"
        );
        assert!(
            !CostAttribution::new("claude-fable-5")
                .with_category(Category::Review)
                .probe,
            "and no other builder turns the flag on"
        );

        let probe = CostAttribution::new("kimi-k2").probe();
        assert!(probe.probe);
        // The flag says what the call was for; it does not change what it
        // bills, so the model still drives the price lookup.
        assert_eq!(probe.model, "kimi-k2");
    }
}
