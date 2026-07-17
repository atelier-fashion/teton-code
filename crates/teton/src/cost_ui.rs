//! The cost meter (AC-4, BR-2).
//!
//! The meter is derived **only** from [`CostRecord`]s the client observes on the
//! `cost_recorded` event stream (BR-2: no estimated or unattributed spend is ever
//! shown as actual). [`CostMeter`] accumulates records; [`CostMeter::report`]
//! rolls them into totals and a per-phase table, and computes the AC-4 savings
//! estimate.
//!
//! ## The savings methodology (OQ-6), mirrored from the daemon
//!
//! Exactly one methodology, identical to the daemon's ledger report: reprice the
//! same input/output token volume of every recorded call at the configured
//! baseline frontier model, then subtract the actual recorded cost. It is a
//! counterfactual, never a measurement — so [`Savings::is_estimate`] is always
//! `true` and the [`Savings::methodology`] string travels with the number and is
//! printed with it, so a reader can never mistake it for measured spend.
//!
//! The authoritative price table lives in the daemon (`tetond::cost`); the CLI
//! carries only the single baseline rate it needs to reprice, which matches the
//! daemon's bundled `anthropic/claude-opus-4` price so the two agree.

use std::collections::BTreeMap;

use teton_protocol::events::CostRecord;
use teton_protocol::Phase;

use crate::render::{LineKind, Surface};

/// Per-million-token divisor; baseline rates are quoted per 1,000,000 tokens.
/// Money stays integer micro-USD end to end; nothing rounds through a float
/// except the final display formatting.
const TOKENS_PER_MTOK: i128 = 1_000_000;

/// The baseline frontier model the savings estimate reprices against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    /// `provider/model` label shown to the user.
    pub label: String,
    /// Micro-USD per 1,000,000 input tokens.
    pub input_micros_per_mtok: i64,
    /// Micro-USD per 1,000,000 output tokens.
    pub output_micros_per_mtok: i64,
}

impl Default for Baseline {
    /// `anthropic/claude-opus-4` at $15 / $75 per million tokens — the same
    /// figures as the daemon's bundled price table, so the client-side estimate
    /// agrees with the daemon's.
    fn default() -> Self {
        Self {
            label: "anthropic/claude-opus-4".to_owned(),
            input_micros_per_mtok: 15_000_000,
            output_micros_per_mtok: 75_000_000,
        }
    }
}

impl Baseline {
    /// What `input`+`output` tokens would cost at this baseline, in micro-USD.
    /// Truncating integer division (conservative: never rounds a cost up).
    #[must_use]
    pub fn cost_micros(&self, input: u64, output: u64) -> i64 {
        let input_cost =
            i128::from(input) * i128::from(self.input_micros_per_mtok) / TOKENS_PER_MTOK;
        let output_cost =
            i128::from(output) * i128::from(self.output_micros_per_mtok) / TOKENS_PER_MTOK;
        let total = input_cost + output_cost;
        total.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }
}

/// A per-phase roll-up row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTotal {
    /// Phase wire-name, or `none` for freeform (phase-less) calls.
    pub phase: String,
    /// Calls attributed to this phase.
    pub calls: u64,
    /// Input tokens summed over the phase.
    pub input_tokens: u64,
    /// Output tokens summed over the phase.
    pub output_tokens: u64,
    /// Recorded spend for the phase, in micro-USD.
    pub usd_micros: i64,
}

/// The AC-4 / OQ-6 savings estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Savings {
    /// The baseline comparator, as `provider/model`.
    pub baseline_model: String,
    /// Actual recorded spend, in micro-USD.
    pub actual_usd_micros: i64,
    /// What the same token volume would cost at the baseline, in micro-USD.
    pub baseline_usd_micros: i64,
    /// `baseline - actual`; the estimated saving (can be zero or negative).
    pub savings_usd_micros: i64,
    /// Always `true`: this is a counterfactual, never a measurement.
    pub is_estimate: bool,
    /// The methodology, verbatim, so it is never shown as measured fact.
    pub methodology: String,
}

/// A full cost summary: totals, the per-phase table, and the savings estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostSummary {
    /// Total recorded spend, in micro-USD.
    pub total_usd_micros: i64,
    /// Total recorded calls.
    pub calls: u64,
    /// Per-phase roll-up, ordered by phase wire-name.
    pub per_phase: Vec<PhaseTotal>,
    /// The savings-vs-frontier estimate.
    pub savings: Savings,
}

/// Accumulates observed [`CostRecord`]s and reports over them.
#[derive(Debug, Default)]
pub struct CostMeter {
    records: Vec<CostRecord>,
}

impl CostMeter {
    /// Ingest one recorded model call.
    pub fn record(&mut self, record: CostRecord) {
        self.records.push(record);
    }

    /// How many records have been observed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no records have been observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Roll the observed records up into a [`CostSummary`], repricing the savings
    /// estimate against `baseline`. Deterministic: phases are sorted by name.
    #[must_use]
    pub fn report(&self, baseline: &Baseline) -> CostSummary {
        let mut total_micros: i64 = 0;
        let mut calls: u64 = 0;
        let mut baseline_micros: i64 = 0;
        let mut by_phase: BTreeMap<String, PhaseAccum> = BTreeMap::new();

        for record in &self.records {
            calls = calls.saturating_add(1);
            total_micros = total_micros.saturating_add(record.usd_micros);
            baseline_micros = baseline_micros
                .saturating_add(baseline.cost_micros(record.input_tokens, record.output_tokens));
            by_phase
                .entry(phase_key(record.phase))
                .or_default()
                .add(record);
        }

        let savings = Savings {
            baseline_model: baseline.label.clone(),
            actual_usd_micros: total_micros,
            baseline_usd_micros: baseline_micros,
            savings_usd_micros: baseline_micros.saturating_sub(total_micros),
            is_estimate: true,
            methodology: methodology_string(baseline),
        };

        CostSummary {
            total_usd_micros: total_micros,
            calls,
            per_phase: by_phase
                .into_iter()
                .map(|(phase, accum)| accum.into_row(phase))
                .collect(),
            savings,
        }
    }
}

/// A running per-phase accumulator.
#[derive(Default)]
struct PhaseAccum {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    usd_micros: i64,
}

impl PhaseAccum {
    fn add(&mut self, record: &CostRecord) {
        self.calls = self.calls.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(record.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(record.output_tokens);
        self.usd_micros = self.usd_micros.saturating_add(record.usd_micros);
    }

    fn into_row(self, phase: String) -> PhaseTotal {
        PhaseTotal {
            phase,
            calls: self.calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            usd_micros: self.usd_micros,
        }
    }
}

/// The phase wire-name used as a grouping key; freeform (no phase) is `none`.
fn phase_key(phase: Option<Phase>) -> String {
    match phase {
        Some(Phase::Spec) => "spec",
        Some(Phase::Architect) => "architect",
        Some(Phase::Implement) => "implement",
        Some(Phase::Review) => "review",
        Some(Phase::Io) => "io",
        Some(Phase::Freeform) => "freeform",
        None => "none",
    }
    .to_owned()
}

/// The savings methodology line, naming the baseline so it is never presented as
/// a measurement.
fn methodology_string(baseline: &Baseline) -> String {
    format!(
        "Estimate, not a measurement. Savings = the same input/output token volume \
         of every recorded call repriced at the baseline frontier model ({}), minus \
         the actual recorded cost.",
        baseline.label
    )
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

/// Render a cost summary to a surface: total, per-phase table, then the labeled
/// savings estimate with its methodology line (AC-4).
pub fn render_summary(summary: &CostSummary, surface: &mut dyn Surface) {
    surface.line(LineKind::Cost, "── cost summary ──");
    surface.line(
        LineKind::Cost,
        &format!(
            "total: {} over {} call(s)",
            format_usd(summary.total_usd_micros),
            summary.calls
        ),
    );

    if summary.per_phase.is_empty() {
        surface.line(LineKind::Cost, "per phase: (no attributed calls)");
    } else {
        surface.line(LineKind::Cost, "per phase:");
        for row in &summary.per_phase {
            surface.line(
                LineKind::Cost,
                &format!(
                    "  {:<10} {:>4} call(s)  {:>7} in / {:>7} out  {}",
                    row.phase,
                    row.calls,
                    row.input_tokens,
                    row.output_tokens,
                    format_usd(row.usd_micros),
                ),
            );
        }
    }

    let s = &summary.savings;
    surface.line(
        LineKind::Cost,
        &format!(
            "estimated savings vs {}: {} (baseline {} − actual {})",
            s.baseline_model,
            format_usd(s.savings_usd_micros),
            format_usd(s.baseline_usd_micros),
            format_usd(s.actual_usd_micros),
        ),
    );
    // The methodology line is mandatory (AC-4): it labels the figure an estimate
    // and states exactly how it was computed.
    let label = if s.is_estimate { "(estimate) " } else { "" };
    surface.line(LineKind::Cost, &format!("  {label}{}", s.methodology));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RecordingSurface;
    use teton_protocol::{ProviderId, SessionId};

    fn record(
        phase: Option<Phase>,
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
        usd_micros: i64,
    ) -> CostRecord {
        CostRecord {
            session_id: SessionId::from("s1"),
            phase,
            provider_id: ProviderId::from(provider),
            model: model.to_owned(),
            input_tokens: input,
            output_tokens: output,
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
    fn empty_meter_reports_zeros_and_still_carries_methodology() {
        let summary = CostMeter::default().report(&Baseline::default());
        assert_eq!(summary.total_usd_micros, 0);
        assert_eq!(summary.calls, 0);
        assert!(summary.per_phase.is_empty());
        assert!(summary.savings.is_estimate);
        assert!(summary.savings.methodology.contains("Estimate"));
        assert!(summary
            .savings
            .methodology
            .contains("anthropic/claude-opus-4"));
    }

    #[test]
    fn baseline_reprices_local_volume_at_the_frontier() {
        // One local (free) implement call: 10k in + 5k out.
        let mut meter = CostMeter::default();
        meter.record(record(
            Some(Phase::Implement),
            "local",
            "qwen2.5-coder-3b",
            10_000,
            5_000,
            0,
        ));
        let summary = meter.report(&Baseline::default());

        assert_eq!(summary.total_usd_micros, 0);
        // 10_000*15 + 5_000*75 = 525_000 micro-USD, matching the daemon's math.
        assert_eq!(summary.savings.baseline_usd_micros, 525_000);
        assert_eq!(summary.savings.savings_usd_micros, 525_000);
    }

    #[test]
    fn per_phase_rollup_is_sorted_and_attributed() {
        let mut meter = CostMeter::default();
        meter.record(record(
            Some(Phase::Review),
            "anthropic",
            "opus",
            1_000,
            500,
            45_000,
        ));
        meter.record(record(
            Some(Phase::Implement),
            "deepseek",
            "coder",
            4_000,
            2_000,
            3_000,
        ));
        meter.record(record(None, "some-vllm", "llama", 800, 200, 100));
        let summary = meter.report(&Baseline::default());

        let phases: Vec<&str> = summary.per_phase.iter().map(|p| p.phase.as_str()).collect();
        // BTreeMap ordering: implement < none < review.
        assert_eq!(phases, vec!["implement", "none", "review"]);
        assert_eq!(summary.calls, 3);
        assert_eq!(summary.total_usd_micros, 48_100);

        let review = summary
            .per_phase
            .iter()
            .find(|p| p.phase == "review")
            .unwrap();
        assert_eq!(review.calls, 1);
        assert_eq!(review.usd_micros, 45_000);
    }

    #[test]
    fn render_summary_prints_totals_table_and_methodology_line() {
        let mut meter = CostMeter::default();
        meter.record(record(
            Some(Phase::Implement),
            "deepseek",
            "coder",
            4_000,
            2_000,
            3_000,
        ));
        let summary = meter.report(&Baseline::default());

        let mut surface = RecordingSurface::new();
        render_summary(&summary, &mut surface);

        // A total line, a per-phase row, a savings line, and the methodology.
        assert!(surface.any_line_contains(LineKind::Cost, "total:"));
        assert!(surface.any_line_contains(LineKind::Cost, "implement"));
        assert!(surface.any_line_contains(LineKind::Cost, "estimated savings"));
        assert!(surface.any_line_contains(LineKind::Cost, "(estimate)"));
        assert!(surface.any_line_contains(LineKind::Cost, "not a measurement"));
    }
}
