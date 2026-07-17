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
use teton_protocol::{Phase, ProviderId, SessionId};
use teton_providers::transport::TransportResponse;

use crate::broadcast::EventBus;

pub use ledger::{CostLedger, LedgerError, LedgerRow};
pub use prices::{ModelPrice, PriceTable};
pub use report::{CostReport, GroupTotals, SavingsEstimate, UnpricedTotals};

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
    pub phase: Option<Phase>,
    /// Concrete model the call bills (drives the price-table lookup).
    pub model: String,
}

impl CostAttribution {
    /// Attribution for `model` with no structured phase (freeform mode).
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            phase: None,
            model: model.into(),
        }
    }

    /// Attribution for `model` in structured-mode `phase`.
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = Some(phase);
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
    fn attribution_builder_sets_phase_and_model() {
        let attr = CostAttribution::new("claude-opus-4").with_phase(Phase::Review);
        assert_eq!(attr.model, "claude-opus-4");
        assert_eq!(attr.phase, Some(Phase::Review));

        let freeform = CostAttribution::new("deepseek-chat");
        assert_eq!(freeform.phase, None);
    }
}
