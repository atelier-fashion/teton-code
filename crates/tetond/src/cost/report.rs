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

use std::collections::BTreeSet;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnpricedTotals {
    /// Number of unpriced calls.
    pub calls: u64,
    /// Input tokens spent on unpriced calls.
    pub input_tokens: u64,
    /// Output tokens spent on unpriced calls.
    pub output_tokens: u64,
    /// Every model in this bucket, by name, deduplicated and ordered (REQ-557
    /// BR-9 / AC-7b).
    ///
    /// The counts above say *how much* went unpriced; without this a user could
    /// not tell *what* to price, and had to go read config or logs to find out.
    /// A `BTreeSet` rather than a `Vec` so the rendering and the tests are
    /// deterministic without sorting at the call site.
    pub models: BTreeSet<String>,
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
        models: BTreeSet::new(),
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
                // BR-9 / AC-7b: name what could not be priced. The row carries
                // the model the provider declared, so the bucket can say which
                // ones need a price entry instead of only how many tokens went
                // uncosted.
                unpriced.models.insert(row.model.clone());
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
            // The rollups group by session, phase, and provider (REQ-544 AC-4);
            // the category rides on the row without changing that shape.
            category: None,
            provider_id: provider.to_owned(),
            model: model.to_owned(),
            input_tokens: input,
            output_tokens: output,
            usd_micros,
        }
    }

    /// REQ-557 AC-7b: the bucket names every model it could not price, so a user
    /// can read off what needs a price entry. Two distinct unpriced models, one
    /// of them called twice, list once each in sorted order.
    #[test]
    fn the_unpriced_bucket_names_every_model_it_could_not_price() {
        let prices = PriceTable::bundled();
        let rows = vec![
            row("s1", None, "vllm", "llama-3-70b", 800, 200, None),
            row("s1", None, "vllm", "llama-3-70b", 100, 50, None),
            row("s1", None, "gateway", "mistral-large", 400, 100, None),
            row(
                "s1",
                Some(Phase::Review),
                "anthropic",
                "claude-opus-4",
                1000,
                500,
                prices.price("claude-opus-4", 1000, 500),
            ),
        ];
        let report = aggregate(&rows, &prices);

        assert_eq!(report.unpriced.calls, 3);
        assert_eq!(
            report.unpriced.models.iter().cloned().collect::<Vec<_>>(),
            vec!["llama-3-70b".to_owned(), "mistral-large".to_owned()],
            "each unpriced model is named exactly once, in a deterministic order"
        );
        // The priced model is NOT in the bucket — this names what needs a price,
        // not what was called.
        assert!(!report.unpriced.models.contains("claude-opus-4"));
    }

    /// A minimal table: one frontier row (so the baseline resolves) and one
    /// genuinely zero-priced row. For tests that need the priced-at-zero vs.
    /// unpriced distinction without depending on the shipped table's contents.
    fn zero_priced_table() -> PriceTable {
        use crate::cost::prices::{Baseline, ModelPrice};
        PriceTable {
            version: 1,
            baseline: Baseline {
                provider_id: "anthropic".to_owned(),
                model: "claude-opus-4".to_owned(),
            },
            models: vec![
                ModelPrice {
                    provider_id: "anthropic".to_owned(),
                    model: "claude-opus-4".to_owned(),
                    input_usd_micros_per_mtok: 15_000_000,
                    output_usd_micros_per_mtok: 75_000_000,
                },
                ModelPrice {
                    provider_id: "promo".to_owned(),
                    model: "free-tier-model".to_owned(),
                    input_usd_micros_per_mtok: 0,
                    output_usd_micros_per_mtok: 0,
                },
            ],
        }
    }

    /// REQ-557 AC-7: an unpriced call is recorded as unpriced, never as a
    /// zero-cost one. The distinction is the whole of BR-9 — a `$0` record reads
    /// as "this was free", which is a claim the meter has no basis for.
    #[test]
    fn an_unpriced_call_is_never_folded_in_as_zero_cost() {
        // The zero-priced entry is built here rather than borrowed from the
        // bundled table: the distinction under test is "priced at zero" vs "no
        // price at all", and it must hold for ANY table, not only for whichever
        // rows happen to ship today. BUG-155 removed the local rows this test
        // used to lean on — they were never used for local traffic (which is
        // unmetered) and, keyed on the model alone, they silently priced remote
        // gateways at zero.
        let prices = zero_priced_table();
        let rows = vec![
            row(
                "s1",
                None,
                "vllm",
                "llama-3-70b",
                1_000_000,
                1_000_000,
                None,
            ),
            // A genuinely free call: the model IS in the table, priced at zero.
            row(
                "s1",
                None,
                "promo",
                "free-tier-model",
                1000,
                500,
                prices.price("free-tier-model", 1000, 500),
            ),
        ];
        let report = aggregate(&rows, &prices);

        // Both contribute zero dollars, but for opposite reasons, and the report
        // keeps them apart: one is priced-at-zero, the other has no price at all.
        assert_eq!(report.total.usd_micros, 0);
        assert_eq!(
            report.total.priced_calls, 1,
            "the zero-priced call IS priced"
        );
        assert_eq!(report.total.unpriced_calls, 1);
        assert_eq!(report.unpriced.calls, 1);
        assert!(report.unpriced.models.contains("llama-3-70b"));
        assert!(
            !report.unpriced.models.contains("free-tier-model"),
            "a model priced at zero is priced, not unpriced"
        );
        // The unpriced call's huge token volume never reaches the savings
        // estimate on either side.
        assert_eq!(report.savings.priced_calls, 1);
    }

    /// REQ-557 AC-7: two providers declaring the same model are priced
    /// identically, from one price entry, and roll up under their own provider
    /// ids. Pre-REQ the second provider went unpriced unless the table carried a
    /// duplicate row keyed to its id.
    #[test]
    fn two_providers_calling_one_model_are_priced_identically() {
        let prices = PriceTable::bundled();
        let cost = prices.price("claude-opus-4", 1000, 500);
        assert!(cost.is_some());
        let rows = vec![
            row(
                "s1",
                None,
                "anthropic-direct",
                "claude-opus-4",
                1000,
                500,
                cost,
            ),
            row(
                "s1",
                None,
                "anthropic-gateway",
                "claude-opus-4",
                1000,
                500,
                cost,
            ),
        ];
        let report = aggregate(&rows, &prices);

        assert_eq!(report.total.priced_calls, 2);
        assert!(report.unpriced.models.is_empty());
        let by_provider: Vec<(&str, i64)> = report
            .per_provider
            .iter()
            .map(|g| (g.key.as_str(), g.usd_micros))
            .collect();
        assert_eq!(
            by_provider,
            vec![
                ("anthropic-direct", cost.unwrap()),
                ("anthropic-gateway", cost.unwrap()),
            ],
            "the same model costs the same whoever served it"
        );
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
        // Two priced calls (opus review + cheap-remote implement) and one unpriced.
        let rows = vec![
            row(
                "s1",
                Some(Phase::Review),
                "anthropic",
                "claude-opus-4",
                1000,
                500,
                prices.price("claude-opus-4", 1000, 500),
            ),
            row(
                "s1",
                Some(Phase::Implement),
                "deepseek",
                "deepseek-chat",
                4000,
                2000,
                prices.price("deepseek-chat", 4000, 2000),
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
        assert_eq!(providers, vec!["anthropic", "deepseek", "some-vllm"]); // sorted
    }

    #[test]
    fn savings_reprices_priced_volume_at_the_frontier() {
        let prices = PriceTable::bundled();
        // One CHEAP REMOTE implement call — the routing-savings story. This was a
        // local call priced from a $0 row until BUG-155 removed those rows (local
        // turns are never metered, and keyed on the model alone the rows priced
        // any remote provider declaring that model at zero). A genuinely cheap
        // remote call tests the same claim and is the case the estimate exists for.
        let cheap_cost = prices.price("deepseek-chat", 10_000, 5000);
        let rows = vec![row(
            "s1",
            Some(Phase::Implement),
            "deepseek",
            "deepseek-chat",
            10_000,
            5000,
            cheap_cost,
        )];
        let report = aggregate(&rows, &prices);

        // Actual: deepseek-chat at $0.27/$1.10 per Mtok.
        //   10_000 * 0.27 + 5_000 * 1.10 = 2_700 + 5_500 = 8_200 micro-USD
        assert_eq!(report.savings.actual_usd_micros, 8_200);
        // Baseline: the same volume at Opus ($15/$75 per Mtok).
        //   10_000 * 15 + 5_000 * 75 = 150_000 + 375_000 = 525_000 micro-USD
        assert_eq!(report.savings.baseline_usd_micros, 525_000);
        assert_eq!(report.savings.savings_usd_micros, 525_000 - 8_200);
        assert_eq!(report.savings.priced_calls, 1);
        assert_eq!(report.savings.baseline_model, "anthropic/claude-opus-4");
    }

    #[test]
    fn using_the_baseline_model_itself_yields_zero_savings() {
        let prices = PriceTable::bundled();
        let cost = prices.price("claude-opus-4", 2000, 1000);
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
