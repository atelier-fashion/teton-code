//! The versioned provider price table (BR-2).
//!
//! Like the local-model catalog (`teton-inference/data/models.toml`), the price
//! table is **data, not code**: a versioned TOML document mapping a
//! `(provider_id, model)` pair to a per-million-token price in integer
//! micro-USD. The daemon can replace it with a newer table (bumping `version`)
//! without a `tetond` release. A default table is embedded in the binary via
//! [`PriceTable::bundled`].
//!
//! ## The unpriced rule (BR-2)
//!
//! A pair that is **absent** from the table is *unpriced*: [`PriceTable::price`]
//! returns `None`, and the caller records the call's token counts with a NULL
//! cost. A price is never guessed for an unknown model — "unknown-price models
//! surface as unpriced tokens, never silently estimated."
//!
//! ## Money is integer micro-USD
//!
//! All arithmetic is integer micro-USD (1e-6 USD); nothing rounds through a
//! float. Per-token math is done in `i128` to leave no room for overflow, then
//! narrowed to the `i64` the wire [`CostRecord`](teton_protocol::events::CostRecord)
//! carries.

use serde::{Deserialize, Serialize};

/// The default price table shipped in the binary. Kept in a data file so it
/// reads as data; validated by the unit tests below.
const BUNDLED_TOML: &str = include_str!("../../data/prices.toml");

/// One micro-USD per USD, i.e. the number of price-table units in one dollar.
const MICROS_PER_USD: i128 = 1_000_000;

/// The per-million-token rate divisor: rates are quoted per 1,000,000 tokens.
const TOKENS_PER_MTOK: i128 = 1_000_000;

/// A `(provider_id, model)` price entry, quoted per million tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Provider id this price applies to (matches `ModelProvider.id`).
    pub provider_id: String,
    /// Concrete model name this price applies to.
    pub model: String,
    /// Micro-USD charged per 1,000,000 input (prompt) tokens.
    pub input_usd_micros_per_mtok: i64,
    /// Micro-USD charged per 1,000,000 output (completion) tokens.
    pub output_usd_micros_per_mtok: i64,
}

impl ModelPrice {
    /// The integer micro-USD cost of `input_tokens` + `output_tokens` at this
    /// entry's rates. Truncating integer division (conservative: never rounds a
    /// cost *up*).
    #[must_use]
    pub fn cost_micros(&self, input_tokens: u64, output_tokens: u64) -> i64 {
        let input =
            i128::from(input_tokens) * i128::from(self.input_usd_micros_per_mtok) / TOKENS_PER_MTOK;
        let output = i128::from(output_tokens) * i128::from(self.output_usd_micros_per_mtok)
            / TOKENS_PER_MTOK;
        // Prices and token counts are bounded far below i64::MAX; the clamp is a
        // belt-and-suspenders guard so a corrupt table can never panic.
        (input + output).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }
}

/// Which model the AC-4 savings estimate reprices against (the all-frontier
/// comparator — OQ-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// Provider id of the baseline model.
    pub provider_id: String,
    /// Baseline model name; must also appear in [`PriceTable::models`].
    pub model: String,
}

/// A versioned set of price entries plus the savings baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTable {
    /// Monotonic table version; lets the daemon detect a newer table.
    pub version: u32,
    /// The all-frontier comparator for the savings estimate.
    pub baseline: Baseline,
    /// The price entries, in author order.
    #[serde(default)]
    pub models: Vec<ModelPrice>,
}

impl PriceTable {
    /// The default price table embedded in the binary.
    ///
    /// # Panics
    /// Panics only if the in-repo `data/prices.toml` is malformed or its
    /// `[baseline]` names a model absent from the table — build-time bugs caught
    /// by this module's own tests, never a runtime condition.
    #[must_use]
    pub fn bundled() -> Self {
        let table = Self::from_toml(BUNDLED_TOML).expect("bundled price table must parse");
        assert!(
            table.baseline_price().is_some(),
            "bundled price table baseline must name a listed model"
        );
        table
    }

    /// Parse a price table from a TOML document.
    ///
    /// # Errors
    /// Returns the underlying TOML deserialization error on malformed input.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// The entry for `(provider_id, model)`, if the table prices it.
    #[must_use]
    pub fn entry(&self, provider_id: &str, model: &str) -> Option<&ModelPrice> {
        self.models
            .iter()
            .find(|m| m.provider_id == provider_id && m.model == model)
    }

    /// The integer micro-USD cost of a call, or `None` when the model is
    /// **unpriced** (BR-2: absent pairs are never guessed a cost).
    #[must_use]
    pub fn price(
        &self,
        provider_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Option<i64> {
        self.entry(provider_id, model)
            .map(|e| e.cost_micros(input_tokens, output_tokens))
    }

    /// The baseline model's price entry, if it is present in the table.
    #[must_use]
    pub fn baseline_price(&self) -> Option<&ModelPrice> {
        self.entry(&self.baseline.provider_id, &self.baseline.model)
    }

    /// The micro-USD the same token volume would have cost at the baseline
    /// frontier model, or `None` if the baseline model is missing from the
    /// table. This is the repricing that powers the AC-4 savings estimate.
    #[must_use]
    pub fn baseline_cost(&self, input_tokens: u64, output_tokens: u64) -> Option<i64> {
        self.baseline_price()
            .map(|e| e.cost_micros(input_tokens, output_tokens))
    }

    /// A human-facing `provider/model` label for the baseline (for the report's
    /// methodology string).
    #[must_use]
    pub fn baseline_label(&self) -> String {
        format!("{}/{}", self.baseline.provider_id, self.baseline.model)
    }
}

impl Default for PriceTable {
    fn default() -> Self {
        Self::bundled()
    }
}

/// Convert a whole-dollar-per-Mtok rate to the table's micro-USD unit. Handy for
/// tests and for authoring notes; not used at runtime.
#[must_use]
pub fn usd_per_mtok_to_micros(usd: i64) -> i64 {
    // MICROS_PER_USD fits i64; the product is bounded for any realistic rate.
    (i128::from(usd) * MICROS_PER_USD).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_table_parses_and_baseline_is_present() {
        let table = PriceTable::bundled();
        assert_eq!(table.version, 1);
        assert_eq!(table.baseline.provider_id, "anthropic");
        assert_eq!(table.baseline.model, "claude-opus-4");
        // The invariant `bundled()` asserts: the baseline names a listed model.
        assert!(table.baseline_price().is_some());
        assert_eq!(table.baseline_label(), "anthropic/claude-opus-4");
    }

    #[test]
    fn known_model_prices_by_the_integer_micro_usd_formula() {
        let table = PriceTable::bundled();
        // Opus: $15/Mtok in, $75/Mtok out. 1000 in + 500 out.
        //   1000 * 15_000_000 / 1_000_000 = 15_000 micro-USD
        //    500 * 75_000_000 / 1_000_000 = 37_500 micro-USD
        let cost = table
            .price("anthropic", "claude-opus-4", 1000, 500)
            .unwrap();
        assert_eq!(cost, 15_000 + 37_500);
    }

    #[test]
    fn unknown_model_is_unpriced_never_guessed() {
        let table = PriceTable::bundled();
        // A registered-but-unlisted OpenAI-compatible endpoint's model.
        assert_eq!(table.price("some-vllm", "llama-3-70b", 1000, 1000), None);
        assert_eq!(table.entry("some-vllm", "llama-3-70b"), None);
    }

    #[test]
    fn local_tier_is_priced_at_zero_not_unpriced() {
        let table = PriceTable::bundled();
        // Local is *priced* (present in the table) at 0 — distinct from an
        // unknown model, which is unpriced (None).
        assert_eq!(
            table.price("local", "qwen2.5-coder-3b", 9999, 9999),
            Some(0)
        );
    }

    #[test]
    fn baseline_cost_reprices_at_the_frontier() {
        let table = PriceTable::bundled();
        // A cheap DeepSeek call's token volume repriced at Opus.
        let baseline = table.baseline_cost(2000, 1000).unwrap();
        // 2000 * 15_000_000/1e6 + 1000 * 75_000_000/1e6 = 30_000 + 75_000
        assert_eq!(baseline, 105_000);
    }

    #[test]
    fn zero_tokens_cost_zero() {
        let table = PriceTable::bundled();
        assert_eq!(table.price("anthropic", "claude-opus-4", 0, 0), Some(0));
    }

    #[test]
    fn round_trips_through_toml() {
        let table = PriceTable::bundled();
        let text = toml::to_string(&table).expect("serialize");
        let back = PriceTable::from_toml(&text).expect("deserialize");
        assert_eq!(table, back);
    }

    #[test]
    fn usd_helper_converts_to_micros() {
        assert_eq!(usd_per_mtok_to_micros(15), 15_000_000);
    }
}
