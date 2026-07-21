//! Cost aggregation and the AC-4 savings estimate (OQ-6).
//!
//! Pure functions over ledger rows: no I/O, no clock, no randomness, so the
//! whole report is deterministic and table-testable. [`aggregate`] rolls the
//! rows up three ways — per session, per phase, per provider — and computes the
//! headline savings-vs-frontier figure the CLI shows at session end.
//!
//! ## What the meter is allowed to claim (BR-2)
//!
//! Everything here derives **only** from recorded [`LedgerRow`]s. Rows for an
//! unpriced model contribute their token counts to an explicit
//! [`UnpricedTotals`] bucket and are excluded from every dollar figure — the
//! meter never invents a cost for a model it has no price for.
//!
//! ## Honesty of the savings figure (OQ-6)
//!
//! The savings estimate is exactly one methodology: **reprice the same token
//! volume of every priced call at the configured baseline frontier model, and
//! subtract the actual recorded cost.** It is a counterfactual, not a
//! measurement, so [`SavingsEstimate::is_estimate`] is always `true` and the
//! [`SavingsEstimate::methodology`] string travels with the number so the CLI
//! can never present it as measured fact.

use serde::Serialize;

use teton_protocol::Phase;

use super::ledger::LedgerRow;
use super::prices::PriceTable;

/// A rolled-up total for one grouping key (a session id, a phase, or a provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupTotals {
    /// The group key (session id, phase wire-name, or provider id).
    pub key: String,
    /// Calls in this group (priced and unpriced).
    pub calls: u64,
    /// Total input tokens in this group.
    pub input_tokens: u64,
    /// Total output tokens in this group.
    pub output_tokens: u64,
    /// Summed cost in micro-USD over the group's **priced** calls only.
    pub usd_micros: i64,
    /// Calls in this group whose model was unpriced (cost unknown).
    pub unpriced_calls: u64,
}

/// Token volume for calls whose model has no price (BR-2: surfaced, never
/// costed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UnpricedTotals {
    /// Number of unpriced calls.
    pub calls: u64,
    /// Input tokens spent on unpriced calls.
    pub input_tokens: u64,
    /// Output tokens spent on unpriced calls.
    pub output_tokens: u64,
}

/// Whole-ledger totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Totals {
    /// All recorded calls.
    pub calls: u64,
    /// All input tokens.
    pub input_tokens: u64,
    /// All output tokens.
    pub output_tokens: u64,
    /// Actual spend in micro-USD (priced calls only).
    pub usd_micros: i64,
    /// Calls that were priced.
    pub priced_calls: u64,
    /// Calls that were unpriced.
    pub unpriced_calls: u64,
}

/// The savings-vs-frontier estimate (AC-4 / OQ-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavingsEstimate {
    /// The baseline comparator, as `provider/model`.
    pub baseline_model: String,
    /// Actual recorded spend over priced calls, in micro-USD.
    pub actual_usd_micros: i64,
    /// What those same calls' token volume would cost at the baseline model.
    pub baseline_usd_micros: i64,
    /// `baseline - actual`; the estimated saving (can be zero, or negative if a
    /// call used a model dearer than the baseline).
    pub savings_usd_micros: i64,
    /// How many priced calls the estimate covers.
    pub priced_calls: u64,
    /// Always `true`: this is a counterfactual, never a measurement.
    pub is_estimate: bool,
    /// The methodology, verbatim, so the CLI never presents it as measured fact.
    pub methodology: String,
}

/// A full cost report: totals, the savings estimate, the unpriced bucket, and
/// the three roll-ups. Serializable so a client can render it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CostReport {
    /// The savings methodology (same string as [`SavingsEstimate::methodology`]),
    /// hoisted to the top level for display prominence.
    pub methodology: String,
    /// Whole-ledger totals.
    pub total: Totals,
    /// The savings-vs-frontier estimate.
    pub savings: SavingsEstimate,
    /// Token volume on unpriced models.
    pub unpriced: UnpricedTotals,
    /// Per-session roll-up, ordered by session id.
    pub per_session: Vec<GroupTotals>,
    /// Per-phase roll-up, ordered by phase wire-name (`none` for freeform calls).
    pub per_phase: Vec<GroupTotals>,
    /// Per-provider roll-up, ordered by provider id.
    pub per_provider: Vec<GroupTotals>,
}

/// A running accumulator for one grouping key.
#[derive(Default)]
struct Accum {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    usd_micros: i64,
    unpriced_calls: u64,
}

impl Accum {
    fn add(&mut self, row: &LedgerRow) {
        self.calls = self.calls.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(row.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(row.output_tokens);
        match row.usd_micros {
            Some(cost) => self.usd_micros = self.usd_micros.saturating_add(cost),
            None => self.unpriced_calls = self.unpriced_calls.saturating_add(1),
        }
    }

    fn into_group(self, key: String) -> GroupTotals {
        GroupTotals {
            key,
            calls: self.calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            usd_micros: self.usd_micros,
            unpriced_calls: self.unpriced_calls,
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

/// Roll `rows` up into a [`CostReport`], pricing the savings baseline against
/// `prices`. Deterministic: group orderings are sorted by key.
#[must_use]
pub fn aggregate(rows: &[LedgerRow], prices: &PriceTable) -> CostReport {
    use std::collections::BTreeMap;

    let mut total = Accum::default();
    let mut unpriced = UnpricedTotals {
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
    };
    let mut by_session: BTreeMap<String, Accum> = BTreeMap::new();
    let mut by_phase: BTreeMap<String, Accum> = BTreeMap::new();
    let mut by_provider: BTreeMap<String, Accum> = BTreeMap::new();

    // Savings sides accumulate over priced calls only.
    let has_baseline = prices.baseline_price().is_some();
    let mut actual_micros: i64 = 0;
    let mut baseline_micros: i64 = 0;
    let mut priced_calls: u64 = 0;

    for row in rows {
        total.add(row);
        by_session
            .entry(row.session_id.clone())
            .or_default()
            .add(row);
        by_phase.entry(phase_key(row.phase)).or_default().add(row);
        by_provider
            .entry(row.provider_id.clone())
            .or_default()
            .add(row);

        match row.usd_micros {
            Some(cost) => {
                priced_calls = priced_calls.saturating_add(1);
                actual_micros = actual_micros.saturating_add(cost);
                // Reprice the same token volume at the baseline frontier model.
                let repriced = prices
                    .baseline_cost(row.input_tokens, row.output_tokens)
                    .unwrap_or(cost);
                baseline_micros = baseline_micros.saturating_add(repriced);
            }
            None => {
                unpriced.calls = unpriced.calls.saturating_add(1);
                unpriced.input_tokens = unpriced.input_tokens.saturating_add(row.input_tokens);
                unpriced.output_tokens = unpriced.output_tokens.saturating_add(row.output_tokens);
            }
        }
    }

    let methodology = methodology_string(prices, has_baseline);
    let savings = SavingsEstimate {
        baseline_model: prices.baseline_label(),
        actual_usd_micros: actual_micros,
        baseline_usd_micros: baseline_micros,
        savings_usd_micros: baseline_micros.saturating_sub(actual_micros),
        priced_calls,
        is_estimate: true,
        methodology: methodology.clone(),
    };

    CostReport {
        methodology,
        total: Totals {
            calls: total.calls,
            input_tokens: total.input_tokens,
            output_tokens: total.output_tokens,
            usd_micros: total.usd_micros,
            priced_calls,
            unpriced_calls: total.unpriced_calls,
        },
        savings,
        unpriced,
        per_session: into_groups(by_session),
        per_phase: into_groups(by_phase),
        per_provider: into_groups(by_provider),
    }
}

fn into_groups(map: std::collections::BTreeMap<String, Accum>) -> Vec<GroupTotals> {
    map.into_iter()
        .map(|(key, accum)| accum.into_group(key))
        .collect()
}

fn methodology_string(prices: &PriceTable, has_baseline: bool) -> String {
    if has_baseline {
        format!(
            "Estimate, not a measurement. Savings = the same input/output token \
             volume of every priced call repriced at the baseline frontier model \
             ({}), minus the actual recorded cost. Unpriced calls (unknown-model \
             tokens) are excluded from both sides and reported separately.",
            prices.baseline_label()
        )
    } else {
        "No savings estimate: the price table names no baseline frontier model, \
         so there is nothing to reprice against."
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        session: &str,
        phase: Option<Phase>,
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
        usd_micros: Option<i64>,
    ) -> LedgerRow {
        LedgerRow {
            session_id: session.to_owned(),
            phase,
            provider_id: provider.to_owned(),
            model: model.to_owned(),
            input_tokens: input,
            output_tokens: output,
            usd_micros,
        }
    }

    #[test]
    fn empty_ledger_reports_zeros_and_no_savings_signal() {
        let report = aggregate(&[], &PriceTable::bundled());
        assert_eq!(report.total.calls, 0);
        assert_eq!(report.savings.actual_usd_micros, 0);
        assert_eq!(report.savings.baseline_usd_micros, 0);
        assert_eq!(report.savings.savings_usd_micros, 0);
        assert!(report.savings.is_estimate);
        assert!(report.per_phase.is_empty());
    }

    #[test]
    fn aggregates_by_session_phase_and_provider() {
        let prices = PriceTable::bundled();
        // Two priced calls (opus review + local implement) and one unpriced.
        let rows = vec![
            row(
                "s1",
                Some(Phase::Review),
                "anthropic",
                "claude-opus-4",
                1000,
                500,
                prices.price("anthropic", "claude-opus-4", 1000, 500),
            ),
            row(
                "s1",
                Some(Phase::Implement),
                "local",
                "qwen2.5-coder-3b",
                4000,
                2000,
                prices.price("local", "qwen2.5-coder-3b", 4000, 2000),
            ),
            row(
                "s2",
                None,
                "some-vllm",
                "llama-3-70b",
                800,
                200,
                None, // unpriced
            ),
        ];
        let report = aggregate(&rows, &prices);

        assert_eq!(report.total.calls, 3);
        assert_eq!(report.total.priced_calls, 2);
        assert_eq!(report.total.unpriced_calls, 1);

        // Unpriced bucket surfaces the unknown-model tokens (BR-2).
        assert_eq!(report.unpriced.calls, 1);
        assert_eq!(report.unpriced.input_tokens, 800);
        assert_eq!(report.unpriced.output_tokens, 200);

        // Per-session: s1 has both priced calls, s2 the unpriced one.
        let s1 = report.per_session.iter().find(|g| g.key == "s1").unwrap();
        assert_eq!(s1.calls, 2);
        assert_eq!(s1.unpriced_calls, 0);
        let s2 = report.per_session.iter().find(|g| g.key == "s2").unwrap();
        assert_eq!(s2.unpriced_calls, 1);
        assert_eq!(s2.usd_micros, 0);

        // Per-phase: review + implement + none (freeform unpriced).
        let phases: Vec<&str> = report.per_phase.iter().map(|g| g.key.as_str()).collect();
        assert!(phases.contains(&"review"));
        assert!(phases.contains(&"implement"));
        assert!(phases.contains(&"none"));

        // Per-provider grouping.
        let providers: Vec<&str> = report.per_provider.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(providers, vec!["anthropic", "local", "some-vllm"]); // sorted
    }

    #[test]
    fn savings_reprices_priced_volume_at_the_frontier() {
        let prices = PriceTable::bundled();
        // One local (free) implement call — the routing-savings story.
        let local_cost = prices.price("local", "qwen2.5-coder-3b", 10_000, 5000);
        let rows = vec![row(
            "s1",
            Some(Phase::Implement),
            "local",
            "qwen2.5-coder-3b",
            10_000,
            5000,
            local_cost,
        )];
        let report = aggregate(&rows, &prices);

        // Actual: local is priced at 0.
        assert_eq!(report.savings.actual_usd_micros, 0);
        // Baseline: 10k in + 5k out at Opus ($15/$75 per Mtok).
        //   10_000 * 15 + 5_000 * 75 = 150_000 + 375_000 = 525_000 micro-USD
        assert_eq!(report.savings.baseline_usd_micros, 525_000);
        assert_eq!(report.savings.savings_usd_micros, 525_000);
        assert_eq!(report.savings.priced_calls, 1);
        assert_eq!(report.savings.baseline_model, "anthropic/claude-opus-4");
    }

    #[test]
    fn using_the_baseline_model_itself_yields_zero_savings() {
        let prices = PriceTable::bundled();
        let cost = prices.price("anthropic", "claude-opus-4", 2000, 1000);
        let rows = vec![row(
            "s1",
            Some(Phase::Spec),
            "anthropic",
            "claude-opus-4",
            2000,
            1000,
            cost,
        )];
        let report = aggregate(&rows, &prices);
        assert_eq!(
            report.savings.actual_usd_micros,
            report.savings.baseline_usd_micros
        );
        assert_eq!(report.savings.savings_usd_micros, 0);
    }

    #[test]
    fn methodology_names_the_baseline_and_flags_estimate() {
        let report = aggregate(&[], &PriceTable::bundled());
        assert!(report.methodology.contains("Estimate"));
        assert!(report.methodology.contains("anthropic/claude-opus-4"));
        assert!(report.savings.is_estimate);
        // The savings payload carries the same methodology string.
        assert_eq!(report.methodology, report.savings.methodology);
    }
}
