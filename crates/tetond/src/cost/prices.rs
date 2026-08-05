//! The versioned provider price table (BR-2).
//!
//! Like the local-model catalog (`teton-inference/data/models.toml`), the price
//! table is **data, not code**: a versioned TOML document mapping a **model** to
//! a per-million-token price in integer micro-USD. The daemon can replace it
//! with a newer table (bumping `version`) without a `tetond` release. A default
//! table is embedded in the binary via [`PriceTable::bundled`].
//!
//! ## The table is a consumer of model identity, never its source (REQ-557)
//!
//! Lookup keys on the model string a provider **declares** it calls
//! ([`ModelProvider::model`](teton_core::entities::ModelProvider)). Before
//! REQ-557 this ran the other way — the router asked this table which model a
//! provider id was billed under, which made a billing table load-bearing for a
//! routing decision and capped a provider at one model forever (ADR-A). Nothing
//! here derives a model identifier any more; it is given one and answers with a
//! price or with nothing.
//!
//! ## The unpriced rule (BR-2)
//!
//! A model **absent** from the table is *unpriced*: [`PriceTable::price`]
//! returns `None`, and the caller records the call's token counts with a NULL
//! cost. A price is never guessed for an unknown model — "unknown-price models
//! surface as unpriced tokens, never silently estimated." The report names those
//! models so a user can see what needs a price (BR-9 / AC-7b).
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

/// One model's price, quoted per million tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// The vendor this entry was authored against (matches `ModelProvider.id`
    /// for the common case). A **label**, not a lookup key: [`PriceTable::entry`]
    /// keys on `model` alone, and this field exists so the baseline can render as
    /// `provider/model` and so a hand-authored table reads unambiguously.
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

    /// The entry for `model`, if the table prices it.
    ///
    /// Keyed on the **model alone** (REQ-557 ADR-A). A model costs what it costs;
    /// the provider is who you bought it from. Keying on `(provider_id, model)`
    /// made a price a property of the pairing, so two providers calling the same
    /// model — the "Opus for design, Sonnet for build" shape REQ-557 BR-3 exists
    /// to enable — needed a duplicate row each, and a provider id absent from the
    /// table left its model unpriced even when the table knew the model perfectly
    /// well. [`ModelPrice::provider_id`] survives as the entry's authoring label
    /// (it renders the baseline as `provider/model`), not as a lookup key.
    #[must_use]
    pub fn entry(&self, model: &str) -> Option<&ModelPrice> {
        self.models.iter().find(|m| m.model == model)
    }

    /// The integer micro-USD cost of a call, or `None` when the model is
    /// **unpriced** (BR-2: an absent model is never guessed a cost).
    #[must_use]
    pub fn price(&self, model: &str, input_tokens: u64, output_tokens: u64) -> Option<i64> {
        self.entry(model)
            .map(|e| e.cost_micros(input_tokens, output_tokens))
    }

    /// The baseline model's price entry, if it is present in the table.
    #[must_use]
    pub fn baseline_price(&self) -> Option<&ModelPrice> {
        self.entry(&self.baseline.model)
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
        let cost = table.price("claude-opus-4", 1000, 500).unwrap();
        assert_eq!(cost, 15_000 + 37_500);
    }

    #[test]
    fn unknown_model_is_unpriced_never_guessed() {
        let table = PriceTable::bundled();
        // A registered-but-unlisted OpenAI-compatible endpoint's model.
        assert_eq!(table.price("llama-3-70b", 1000, 1000), None);
        assert_eq!(table.entry("llama-3-70b"), None);
    }

    #[test]
    fn local_tier_is_priced_at_zero_not_unpriced() {
        let table = PriceTable::bundled();
        // Local is *priced* (present in the table) at 0 — distinct from an
        // unknown model, which is unpriced (None).
        assert_eq!(table.price("qwen2.5-coder-3b", 9999, 9999), Some(0));
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
        assert_eq!(table.price("claude-opus-4", 0, 0), Some(0));
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

    /// REQ-557 AC-7: two providers declaring the same model are priced from ONE
    /// entry. Pre-REQ the lookup keyed on `(provider_id, model)`, so this needed
    /// a duplicate row per provider — and the second provider silently went
    /// unpriced until somebody noticed and added one.
    #[test]
    fn two_providers_calling_one_model_price_from_a_single_entry() {
        let table = PriceTable::bundled();
        // `deepseek-chat` is authored once, under provider id "deepseek". Two
        // providers — say a direct account and a proxy — both declare it.
        let direct = table.price("deepseek-chat", 1000, 200);
        let proxy = table.price("deepseek-chat", 1000, 200);
        assert_eq!(direct, proxy);
        assert_eq!(direct, Some(270 + 220));
        // And exactly one row backs both.
        assert_eq!(
            table
                .models
                .iter()
                .filter(|m| m.model == "deepseek-chat")
                .count(),
            1
        );
    }

    /// A provider id absent from the table no longer suppresses a price the table
    /// plainly holds. This is the other half of the pre-REQ keying defect: an
    /// in-house gateway calling `claude-opus-4` was billed as unpriced because
    /// the table had no row for the *gateway's* id.
    #[test]
    fn a_model_is_priced_whoever_serves_it() {
        let table = PriceTable::bundled();
        assert_eq!(
            table.price("claude-opus-4", 1000, 500),
            Some(15_000 + 37_500),
            "a known model must price regardless of which provider declared it"
        );
    }

    /// Model-keyed lookup makes a duplicate model name ambiguous — `find` would
    /// silently take the first. The bundled table must not contain one.
    #[test]
    fn the_bundled_table_names_each_model_once() {
        let table = PriceTable::bundled();
        let mut seen = std::collections::BTreeSet::new();
        for entry in &table.models {
            assert!(
                seen.insert(entry.model.clone()),
                "model {:?} is priced twice; lookup keys on the model alone, so a \
                 duplicate makes the price ambiguous",
                entry.model
            );
        }
    }
}
