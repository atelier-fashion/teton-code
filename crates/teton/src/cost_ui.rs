//! The cost meter (AC-4, BR-2).
//!
//! Two responsibilities, both derived **only** from the daemon's authoritative
//! numbers (BR-2: no estimated or unattributed spend is ever shown as actual):
//!
//! 1. [`CostMeter`] observes the [`CostRecord`]s that stream on the
//!    `cost_recorded` event during a live session, purely to count what happened
//!    this session (e.g. "recorded N model call(s)"). It performs no repricing.
//! 2. [`render_report_view`] renders the daemon's authoritative
//!    [`CostReportView`] — totals, the per-phase / per-provider roll-ups, and the
//!    savings-vs-frontier estimate with its methodology line (AC-4).
//!
//! ## Single source of truth for the savings estimate (REQ-544 M-7)
//!
//! The savings methodology (OQ-6) and its baseline frontier price live in the
//! daemon (`tetond::cost`), which computes the estimate over its persisted ledger
//! and projects it onto [`CostReportView`]. The CLI does **not** carry a baseline
//! price of its own and does **not** reprice anything — it renders the daemon's
//! `cost/query` report directly, so there is exactly one place a savings number is
//! computed. (The previous client-side `Baseline` literal — hand-synced to
//! `prices.toml` — was a second source of truth and has been removed.)

use teton_protocol::events::CostRecord;
use teton_protocol::methods::CostReportView;

use crate::render::{LineKind, Surface};

/// Accumulates the [`CostRecord`]s observed on the live event stream during a
/// session, so the CLI can report how many model calls happened this session.
///
/// It deliberately does **not** aggregate spend or estimate savings: the
/// authoritative totals and the savings figure come from the daemon's
/// `cost/query` report ([`render_report_view`]). This type only counts.
#[derive(Debug, Default)]
pub struct CostMeter {
    records: Vec<CostRecord>,
}

impl CostMeter {
    /// Ingest one recorded model call observed on the event stream.
    pub fn record(&mut self, record: CostRecord) {
        self.records.push(record);
    }

    /// How many records have been observed this session.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no records have been observed this session.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Format integer micro-USD as an exact dollar string, e.g. `525000` →
/// `$0.525000`. No float rounding — the six fractional digits are the micro part.
#[must_use]
pub fn format_usd(micros: i64) -> String {
    let negative = micros < 0;
    let magnitude = micros.unsigned_abs();
    let dollars = magnitude / 1_000_000;
    let fraction = magnitude % 1_000_000;
    let sign = if negative { "-" } else { "" };
    format!("{sign}${dollars}.{fraction:06}")
}

/// Render the daemon's authoritative [`CostReportView`] to a surface: total,
/// priced/unpriced call counts, the per-phase and per-provider tables, then the
/// labeled savings estimate with its methodology line (AC-4).
///
/// Every figure here — including the baseline and savings — comes verbatim from
/// the daemon's `cost/query` report; the CLI computes none of it (REQ-544 M-7).
pub fn render_report_view(report: &CostReportView, surface: &mut dyn Surface) {
    surface.line(LineKind::Cost, "── cost summary ──");
    surface.line(
        LineKind::Cost,
        &format!(
            "total: {} over {} call(s)",
            format_usd(report.total_usd_micros),
            report.total_calls
        ),
    );
    if report.unpriced_calls > 0 {
        surface.line(
            LineKind::Cost,
            &format!(
                "  ({} priced, {} unpriced — an unpriced call is recorded but never assigned a guessed cost)",
                report.priced_calls, report.unpriced_calls
            ),
        );
    }

    render_group(surface, "per phase", &report.per_phase);
    render_group(surface, "per provider", &report.per_provider);

    surface.line(
        LineKind::Cost,
        &format!(
            "estimated savings vs {}: {} (baseline {} − actual {})",
            report.baseline_model,
            format_usd(report.savings_usd_micros),
            format_usd(report.baseline_usd_micros),
            format_usd(report.total_usd_micros),
        ),
    );
    // The methodology line is mandatory (AC-4): it labels the figure an estimate
    // and states exactly how the daemon computed it.
    surface.line(
        LineKind::Cost,
        &format!("  (estimate) {}", report.methodology),
    );
}

/// Render one named roll-up group (per-phase or per-provider) from the daemon's
/// report, or a "(no attributed calls)" note when the group is empty.
fn render_group(
    surface: &mut dyn Surface,
    label: &str,
    groups: &[teton_protocol::methods::CostGroupView],
) {
    if groups.is_empty() {
        surface.line(LineKind::Cost, &format!("{label}: (no attributed calls)"));
        return;
    }
    surface.line(LineKind::Cost, &format!("{label}:"));
    for row in groups {
        surface.line(
            LineKind::Cost,
            &format!(
                "  {:<12} {:>4} call(s)  {:>7} in / {:>7} out  {}",
                row.key,
                row.calls,
                row.input_tokens,
                row.output_tokens,
                format_usd(row.usd_micros),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RecordingSurface;
    use teton_protocol::methods::CostGroupView;
    use teton_protocol::{ProviderId, SessionId};

    fn record(phase: Option<teton_protocol::Phase>, usd_micros: i64) -> CostRecord {
        CostRecord {
            session_id: SessionId::from("s1"),
            phase,
            provider_id: ProviderId::from("deepseek"),
            model: "coder".to_owned(),
            input_tokens: 100,
            output_tokens: 50,
            usd_micros,
        }
    }

    #[test]
    fn format_usd_is_exact_and_handles_sign() {
        assert_eq!(format_usd(525_000), "$0.525000");
        assert_eq!(format_usd(0), "$0.000000");
        assert_eq!(format_usd(1_500_000), "$1.500000");
        assert_eq!(format_usd(-42), "-$0.000042");
    }

    #[test]
    fn cost_meter_only_counts_observed_records() {
        let mut meter = CostMeter::default();
        assert!(meter.is_empty());
        meter.record(record(Some(teton_protocol::Phase::Implement), 3_000));
        meter.record(record(None, 100));
        assert_eq!(meter.len(), 2);
        assert!(!meter.is_empty());
    }

    #[test]
    fn render_report_view_prints_daemon_totals_savings_and_methodology() {
        // The CLI renders the daemon's authoritative report verbatim — it does not
        // reprice or recompute the savings (REQ-544 M-7).
        let report = CostReportView {
            total_usd_micros: 3_000,
            total_calls: 2,
            priced_calls: 1,
            unpriced_calls: 1,
            savings_usd_micros: 522_000,
            baseline_usd_micros: 525_000,
            baseline_model: "anthropic/claude-opus-4".to_owned(),
            methodology: "Estimate, not a measurement. Repriced at the baseline.".to_owned(),
            per_phase: vec![CostGroupView {
                key: "implement".to_owned(),
                calls: 1,
                input_tokens: 4_000,
                output_tokens: 2_000,
                usd_micros: 3_000,
            }],
            per_provider: vec![CostGroupView {
                key: "deepseek".to_owned(),
                calls: 1,
                input_tokens: 4_000,
                output_tokens: 2_000,
                usd_micros: 3_000,
            }],
        };

        let mut surface = RecordingSurface::new();
        render_report_view(&report, &mut surface);

        assert!(surface.any_line_contains(LineKind::Cost, "total:"));
        assert!(surface.any_line_contains(LineKind::Cost, "implement"));
        assert!(surface.any_line_contains(LineKind::Cost, "deepseek"));
        assert!(surface.any_line_contains(LineKind::Cost, "unpriced"));
        assert!(surface.any_line_contains(LineKind::Cost, "estimated savings"));
        assert!(surface.any_line_contains(LineKind::Cost, "(estimate)"));
        assert!(surface.any_line_contains(LineKind::Cost, "not a measurement"));
        // The baseline model and savings come from the daemon's report, verbatim.
        assert!(surface.any_line_contains(LineKind::Cost, "anthropic/claude-opus-4"));
        assert!(surface.any_line_contains(LineKind::Cost, "$0.522000"));
    }
}
