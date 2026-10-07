//! Cost aggregation and the AC-4 savings estimate (OQ-6).
//!
//! Pure functions over ledger rows: no I/O, no clock, no randomness, so the
//! whole report is deterministic and table-testable. [`aggregate`] rolls the
//! provider-call rows up three ways — per session, per phase, per provider —
//! computes the headline savings-vs-frontier figure the CLI shows at session
//! end, and rolls the `web_lookups` rows up per session alongside them
//! ([`WebTotals`], REQ-563 BR-7). The two roll-ups never merge: a lookup is not
//! a call and must not be counted as one.
//!
//! ## A turn and its children (REQ-623 BR-8)
//!
//! A child's calls are ordinary calls — the parent pays, so every roll-up above
//! counts them exactly as it counts any other. [`TurnTotals`] is an additional
//! *view*, never a second count: rows stamped with a `parent_turn_id` group by
//! `(session, turn)`, the turn's own calls apart from one line per child, and
//! the turn's total is the two summed. A row with no ids touches only the
//! existing roll-ups, which are byte-identical to what they were before the
//! columns existed.
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

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use teton_protocol::agent::ChildId;
use teton_protocol::{Phase, TurnId};

use super::ledger::{LedgerRow, WebLookupRow};
use super::prices::PriceTable;

/// A rolled-up total for one grouping key (a session id, a phase, a provider,
/// or — inside a [`TurnTotals`] — a turn id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupTotals {
    /// The group key (session id, phase wire-name, provider id, or turn id).
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

/// Per-session web-lookup totals (REQ-563 BR-7 / AC-6).
///
/// A separate roll-up rather than columns on [`GroupTotals`], for the reason
/// `web_lookups` is a separate table: a lookup has no tokens and no cost to add
/// to a call's, and "calls" and "lookups" are different counts a reader must not
/// see summed. Only the per-session grouping exists because only the session is
/// a key both tables share — a lookup has no phase, no provider, and no model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebTotals {
    /// The session id.
    pub key: String,
    /// Lookups this session performed, whatever their outcome — blocked,
    /// refused, and cache-served ones included (BR-7: every lookup lands here).
    pub lookups: u64,
    /// Bytes those lookups brought back. `0` from every ending that transferred
    /// nothing, so this is content received and not traffic attempted.
    pub bytes_in: u64,
}

/// One child's line beneath its parent turn (REQ-623 BR-8 / AC-12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildTotals {
    /// The child's full id, `"<call_id>/<name>"` — the key a client matches on.
    pub child_id: ChildId,
    /// The label the line is shown under: [`ChildId::name`], the id's one
    /// sanctioned accessor (the part after its first `/`).
    ///
    /// The ledger has no name column, so the id is the one place the label can
    /// come from. A display label only: [`Self::child_id`] stays the identity.
    pub name: String,
    /// Where the child's calls went, as `provider/model`. A child rerouted
    /// mid-run lists each distinct route once, in call order, joined by `" → "`
    /// — so a reroute shows rather than being hidden behind the last route.
    pub route: String,
    /// Calls the child made (priced and unpriced).
    pub calls: u64,
    /// Input tokens over the child's calls.
    pub input_tokens: u64,
    /// Output tokens over the child's calls.
    pub output_tokens: u64,
    /// The child's cost in micro-USD, over its **priced** calls only.
    pub usd_micros: i64,
    /// Of [`Self::calls`], how many were unpriced (cost unknown).
    pub unpriced_calls: u64,
}

/// A parent turn that dispatched children, with each child's spend nested
/// beneath its own (REQ-623 BR-8 / AC-12).
///
/// A view over rows every other roll-up already counted — never added to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnTotals {
    /// The session the turn ran in.
    pub session_id: String,
    /// The parent turn.
    pub turn_id: TurnId,
    /// The turn's own calls: rows stamped with this turn and no child. Keyed by
    /// the turn id. All zero where the turn's own calls carried no turn stamp.
    pub own: GroupTotals,
    /// One line per child, in the order the children first spent.
    pub children: Vec<ChildTotals>,
    /// [`Self::own`] plus every child's — the parent turn's total, keyed by the
    /// turn id.
    pub total: GroupTotals,
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
    /// Reasoning tokens summed over the calls that reported a split, or `None`
    /// when **no** call did (REQ-559 BR-11).
    ///
    /// `None` renders as "unreported"; a `0` would claim every provider did no
    /// thinking, which is displaying an estimate as an actual (REQ-544 BR-2).
    /// A **subset** of `output_tokens`, never added to it.
    pub reasoning_tokens: Option<u64>,
    /// How many calls reported a reasoning split, so a partial figure can say
    /// it is partial rather than reading as a whole-ledger total.
    pub calls_reporting_reasoning: u64,
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
    /// Per-session web-lookup roll-up, ordered by session id (REQ-563 AC-6).
    ///
    /// Sessions with no lookups do not appear — the common case is every
    /// session, since web lookup is off by default (BR-1), and an empty list is
    /// the honest shape of "this build did no web lookups" rather than a wall of
    /// zeroes.
    pub web_per_session: Vec<WebTotals>,
    /// How many of [`Totals::calls`] were connection tests (REQ-581 BR-5).
    ///
    /// A **subset** of the total, never a tally added to it: a probe is an
    /// ordinary model call, sent down the same path and priced from the same
    /// table, and every roll-up above already counts it as one. This field only
    /// buys the sentence `teton cost` can then print — so a user reading a call
    /// they asked no question for does not read it as a turn.
    ///
    /// A whole-ledger count rather than a column on [`GroupTotals`]: the
    /// question is "did any of this spend come from testing", which is asked of
    /// the ledger and not of a session, a phase, or a provider.
    pub probe_calls: u64,
    /// Every parent turn that dispatched at least one child, with the children
    /// nested beneath it (REQ-623 BR-8 / AC-12), in the order the turns first
    /// spent.
    ///
    /// Ledger order rather than key order: turn ids come off a counter, and
    /// sorting `turn-10` before `turn-2` would misreport the sequence. A turn
    /// with no children does not appear — every other roll-up already says all
    /// there is to say about it — so a ledger with no children has an empty
    /// list, not a row per turn.
    pub per_turn: Vec<TurnTotals>,
}

/// A running accumulator for one grouping key.
#[derive(Default)]
struct Accum {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    usd_micros: i64,
    unpriced_calls: u64,
    /// REQ-559 BR-11: reasoning tokens summed over the rows that **reported**
    /// one, and the count of those rows. Both are needed: a bare sum over a
    /// mixed ledger reads as a whole-ledger figure when it is a partial one,
    /// and presenting a partial as a total is the estimate-as-actual REQ-544
    /// BR-2 forbids.
    reasoning_tokens: u64,
    calls_reporting_reasoning: u64,
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
        // Never summed into `output_tokens` — a subset, not an addition (BR-10).
        if let Some(reasoning) = row.reasoning_tokens {
            self.reasoning_tokens = self.reasoning_tokens.saturating_add(reasoning);
            self.calls_reporting_reasoning = self.calls_reporting_reasoning.saturating_add(1);
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

/// A running accumulator for one child under one turn.
struct ChildAccum {
    child_id: ChildId,
    accum: Accum,
    /// Distinct `provider/model` routes, in the order the child first used them.
    routes: Vec<String>,
}

/// A running accumulator for one `(session, turn)`.
struct TurnAccum {
    session_id: String,
    turn_id: TurnId,
    own: Accum,
    total: Accum,
    children: Vec<ChildAccum>,
}

impl TurnAccum {
    fn add(&mut self, row: &LedgerRow) {
        self.total.add(row);
        let Some(child_id) = &row.child_id else {
            self.own.add(row);
            return;
        };
        let index = match self.children.iter().position(|c| &c.child_id == child_id) {
            Some(index) => index,
            None => {
                self.children.push(ChildAccum {
                    child_id: child_id.clone(),
                    accum: Accum::default(),
                    routes: Vec::new(),
                });
                self.children.len() - 1
            }
        };
        let child = &mut self.children[index];
        child.accum.add(row);
        let route = format!("{}/{}", row.provider_id, row.model);
        if !child.routes.contains(&route) {
            child.routes.push(route);
        }
    }

    fn into_totals(self) -> TurnTotals {
        let key = self.turn_id.0.clone();
        TurnTotals {
            session_id: self.session_id,
            own: self.own.into_group(key.clone()),
            total: self.total.into_group(key),
            turn_id: self.turn_id,
            children: self
                .children
                .into_iter()
                .map(|child| ChildTotals {
                    name: child.child_id.name().to_owned(),
                    route: child.routes.join(" → "),
                    calls: child.accum.calls,
                    input_tokens: child.accum.input_tokens,
                    output_tokens: child.accum.output_tokens,
                    usd_micros: child.accum.usd_micros,
                    unpriced_calls: child.accum.unpriced_calls,
                    child_id: child.child_id,
                })
                .collect(),
        }
    }
}

/// Group the turn-stamped rows by `(session, turn)` in ledger order, keeping
/// only the turns that dispatched a child (REQ-623 BR-8).
///
/// A child row with no `parent_turn_id` cannot be nested anywhere and is left
/// to the roll-ups that already count it; the only builder that sets a child id
/// sets the turn too, so such a row comes only from a hand-edited store.
fn per_turn(rows: &[LedgerRow]) -> Vec<TurnTotals> {
    let mut index: BTreeMap<(&str, &TurnId), usize> = BTreeMap::new();
    let mut turns: Vec<TurnAccum> = Vec::new();
    for row in rows {
        let Some(turn_id) = &row.parent_turn_id else {
            continue;
        };
        let slot = *index
            .entry((row.session_id.as_str(), turn_id))
            .or_insert_with(|| {
                turns.push(TurnAccum {
                    session_id: row.session_id.clone(),
                    turn_id: turn_id.clone(),
                    own: Accum::default(),
                    total: Accum::default(),
                    children: Vec::new(),
                });
                turns.len() - 1
            });
        turns[slot].add(row);
    }
    turns
        .into_iter()
        .filter(|turn| !turn.children.is_empty())
        .map(TurnAccum::into_totals)
        .collect()
}

/// The phase wire-name used as a grouping key; freeform (no phase) is `none`.
fn phase_key(phase: Option<Phase>) -> String {
    match phase {
        Some(Phase::Spec) => "spec",
        Some(Phase::Architect) => "architect",
        Some(Phase::Implement) => "implement",
        Some(Phase::Review) => "review",
        Some(Phase::Io) => "io",
        // Every phaseless call lands here: freeform turns, which have always
        // recorded `phase: NULL`, and rows from a build that still wrote the
        // retired `"freeform"` string (ADR-G).
        None => "none",
    }
    .to_owned()
}

/// A running accumulator for one session's web lookups.
#[derive(Default)]
struct WebAccum {
    lookups: u64,
    bytes_in: u64,
}

/// Roll `rows` (provider calls) and `web_rows` (web lookups) up into a
/// [`CostReport`], pricing the savings baseline against `prices`.
/// Deterministic: group orderings are sorted by key.
///
/// The two row kinds arrive as separate slices, and stay separate in the report:
/// they answer different questions and a reader must never see a lookup counted
/// as a call (REQ-563 D-7).
#[must_use]
pub fn aggregate(rows: &[LedgerRow], web_rows: &[WebLookupRow], prices: &PriceTable) -> CostReport {
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
    // REQ-581 BR-5: counted alongside the roll-ups, not instead of them — the
    // probe is added to every total above as the ordinary call it is.
    let mut probe_calls: u64 = 0;

    for row in rows {
        total.add(row);
        if row.probe {
            probe_calls = probe_calls.saturating_add(1);
        }
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

    let mut web_by_session: BTreeMap<String, WebAccum> = BTreeMap::new();
    for row in web_rows {
        let accum = web_by_session.entry(row.session_id.clone()).or_default();
        accum.lookups = accum.lookups.saturating_add(1);
        accum.bytes_in = accum.bytes_in.saturating_add(row.bytes_in);
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
            reasoning_tokens: (total.calls_reporting_reasoning > 0)
                .then_some(total.reasoning_tokens),
            calls_reporting_reasoning: total.calls_reporting_reasoning,
        },
        savings,
        unpriced,
        per_session: into_groups(by_session),
        per_phase: into_groups(by_phase),
        per_provider: into_groups(by_provider),
        web_per_session: web_by_session
            .into_iter()
            .map(|(key, accum)| WebTotals {
                key,
                lookups: accum.lookups,
                bytes_in: accum.bytes_in,
            })
            .collect(),
        probe_calls,
        per_turn: per_turn(rows),
    }
}

fn into_groups(map: BTreeMap<String, Accum>) -> Vec<GroupTotals> {
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
            cached_tokens: None,
            reasoning_tokens: None,
            // A turn. The probe rows a test needs are built as
            // `LedgerRow { probe: true, ..row(..) }`, so the default here stays
            // the overwhelmingly common case.
            probe: false,
            // Attributed to no turn and no child — every row before REQ-623.
            // The rows a nesting test needs are built with `in_turn`/`by_child`.
            child_id: None,
            parent_turn_id: None,
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
                "claude-fable-5",
                1000,
                500,
                prices.price("claude-fable-5", 1000, 500),
            ),
        ];
        let report = aggregate(&rows, &[], &prices);

        assert_eq!(report.unpriced.calls, 3);
        assert_eq!(
            report.unpriced.models.iter().cloned().collect::<Vec<_>>(),
            vec!["llama-3-70b".to_owned(), "mistral-large".to_owned()],
            "each unpriced model is named exactly once, in a deterministic order"
        );
        // The priced model is NOT in the bucket — this names what needs a price,
        // not what was called.
        assert!(!report.unpriced.models.contains("claude-fable-5"));
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
        let report = aggregate(&rows, &[], &prices);

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
        let cost = prices.price("claude-fable-5", 1000, 500);
        assert!(cost.is_some());
        let rows = vec![
            row(
                "s1",
                None,
                "anthropic-direct",
                "claude-fable-5",
                1000,
                500,
                cost,
            ),
            row(
                "s1",
                None,
                "anthropic-gateway",
                "claude-fable-5",
                1000,
                500,
                cost,
            ),
        ];
        let report = aggregate(&rows, &[], &prices);

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

    /// REQ-581 BR-5: a probe is counted apart and *also* counted as the call it
    /// is. Both halves matter — a probe missing from the totals would under-report
    /// real spend, and a probe indistinguishable from a turn is the reading
    /// problem this REQ exists to fix.
    #[test]
    fn a_probe_is_counted_apart_and_still_counted_as_a_call() {
        let prices = PriceTable::bundled();
        let cost = prices.price("claude-fable-5", 1000, 500);
        let turn = row("s1", None, "anthropic", "claude-fable-5", 1000, 500, cost);
        let probe = LedgerRow {
            probe: true,
            ..row("s1", None, "kimi", "kimi-k2", 8, 4, None)
        };
        let report = aggregate(&[turn, probe], &[], &prices);

        assert_eq!(report.probe_calls, 1, "one of the two rows was a test");
        assert_eq!(
            report.total.calls, 2,
            "the probe is spend and stays in the total — the count is a subset, \
             never a deduction"
        );
        assert_eq!(
            report.total.usd_micros,
            cost.unwrap_or(0),
            "and it is priced by the same table as any other call"
        );

        // A ledger of turns reports no probes rather than omitting the figure.
        let turns_only = aggregate(
            &[row("s1", None, "anthropic", "claude-fable-5", 10, 5, cost)],
            &[],
            &prices,
        );
        assert_eq!(turns_only.probe_calls, 0);
    }

    #[test]
    fn empty_ledger_reports_zeros_and_no_savings_signal() {
        let report = aggregate(&[], &[], &PriceTable::bundled());
        assert_eq!(report.total.calls, 0);
        assert_eq!(report.probe_calls, 0);
        assert_eq!(report.savings.actual_usd_micros, 0);
        assert_eq!(report.savings.baseline_usd_micros, 0);
        assert_eq!(report.savings.savings_usd_micros, 0);
        assert!(report.savings.is_estimate);
        assert!(report.per_phase.is_empty());
    }

    #[test]
    fn aggregates_by_session_phase_and_provider() {
        let prices = PriceTable::bundled();
        // Two priced calls (frontier review + cheap-remote implement) and one unpriced.
        let rows = vec![
            row(
                "s1",
                Some(Phase::Review),
                "anthropic",
                "claude-fable-5",
                1000,
                500,
                prices.price("claude-fable-5", 1000, 500),
            ),
            row(
                "s1",
                Some(Phase::Implement),
                "deepseek",
                "deepseek-v4-pro",
                4000,
                2000,
                prices.price("deepseek-v4-pro", 4000, 2000),
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
        let report = aggregate(&rows, &[], &prices);

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
        let cheap_cost = prices.price("deepseek-v4-pro", 10_000, 5000);
        let rows = vec![row(
            "s1",
            Some(Phase::Implement),
            "deepseek",
            "deepseek-v4-pro",
            10_000,
            5000,
            cheap_cost,
        )];
        let report = aggregate(&rows, &[], &prices);

        // Actual: deepseek-v4-pro at the peak-ceiling $1.32/$3.96 per Mtok
        // (time-of-day convention documented in prices.toml).
        //   10_000 * 1.32 + 5_000 * 3.96 = 13_200 + 19_800 = 33_000 micro-USD
        assert_eq!(report.savings.actual_usd_micros, 33_000);
        // Baseline: the same volume at Fable ($10/$50 per Mtok).
        //   10_000 * 10 + 5_000 * 50 = 100_000 + 250_000 = 350_000 micro-USD
        assert_eq!(report.savings.baseline_usd_micros, 350_000);
        assert_eq!(report.savings.savings_usd_micros, 350_000 - 33_000);
        assert_eq!(report.savings.priced_calls, 1);
        assert_eq!(report.savings.baseline_model, "anthropic/claude-fable-5");
    }

    #[test]
    fn using_the_baseline_model_itself_yields_zero_savings() {
        let prices = PriceTable::bundled();
        let cost = prices.price("claude-fable-5", 2000, 1000);
        let rows = vec![row(
            "s1",
            Some(Phase::Spec),
            "anthropic",
            "claude-fable-5",
            2000,
            1000,
            cost,
        )];
        let report = aggregate(&rows, &[], &prices);
        assert_eq!(
            report.savings.actual_usd_micros,
            report.savings.baseline_usd_micros
        );
        assert_eq!(report.savings.savings_usd_micros, 0);
    }

    /// REQ-563 AC-6: a session with recorded lookups reports how many it made
    /// and how many bytes came back — and none of that leaks into the call
    /// counts, which is the whole reason `web_lookups` is a sibling table.
    #[test]
    fn lookups_roll_up_per_session_without_touching_the_call_totals() {
        use teton_protocol::events::{WebLookupKind, WebLookupOutcome};

        let prices = PriceTable::bundled();
        let calls = vec![row(
            "s1",
            Some(Phase::Implement),
            "deepseek",
            "deepseek-v4-pro",
            4000,
            2000,
            prices.price("deepseek-v4-pro", 4000, 2000),
        )];
        let lookup = |session: &str, outcome, bytes_in| WebLookupRow {
            session_id: session.to_owned(),
            kind: WebLookupKind::Fetch,
            host: "docs.rs".to_owned(),
            bytes_in,
            duration_ms: 120,
            outcome,
            usd_micros: Some(0),
        };
        let lookups = vec![
            lookup("s1", WebLookupOutcome::Completed, 4096),
            lookup("s1", WebLookupOutcome::CacheHit, 4096),
            // BR-7: a refused lookup is still a lookup. It brought back nothing,
            // so it adds to the count and not to the bytes.
            lookup("s1", WebLookupOutcome::RefusedDomain, 0),
            lookup("s2", WebLookupOutcome::Completed, 100),
        ];

        let report = aggregate(&calls, &lookups, &prices);

        let s1 = report
            .web_per_session
            .iter()
            .find(|w| w.key == "s1")
            .expect("s1 made lookups");
        assert_eq!(s1.lookups, 3);
        assert_eq!(s1.bytes_in, 8192);
        let s2 = report
            .web_per_session
            .iter()
            .find(|w| w.key == "s2")
            .expect("s2 made a lookup");
        assert_eq!(s2.lookups, 1);
        assert_eq!(s2.bytes_in, 100);
        // Ordered by session id, deterministically.
        let keys: Vec<&str> = report
            .web_per_session
            .iter()
            .map(|w| w.key.as_str())
            .collect();
        assert_eq!(keys, vec!["s1", "s2"]);

        // The call side is untouched: one call, in one session. A session that
        // only looked things up appears in the web roll-up and NOT in the call
        // roll-up, because it made no calls.
        assert_eq!(report.total.calls, 1);
        let call_sessions: Vec<&str> = report.per_session.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(call_sessions, vec!["s1"], "a lookup is not a call");
    }

    /// The default state (BR-1: web lookup is off) reports no web roll-up at
    /// all, rather than a row of zeroes per session.
    #[test]
    fn a_ledger_with_no_lookups_reports_no_web_rollup() {
        let prices = PriceTable::bundled();
        let rows = vec![row(
            "s1",
            None,
            "deepseek",
            "deepseek-v4-pro",
            10,
            5,
            Some(1),
        )];
        let report = aggregate(&rows, &[], &prices);
        assert!(report.web_per_session.is_empty());
        assert_eq!(report.per_session.len(), 1);
    }

    /// A parent turn's own call, stamped with its turn.
    fn in_turn(turn: &str, row: LedgerRow) -> LedgerRow {
        LedgerRow {
            parent_turn_id: Some(TurnId::from(turn)),
            ..row
        }
    }

    /// A child's call: both ids, as `CostAttribution::for_child` stamps them.
    fn by_child(call_id: &str, name: &str, turn: &str, row: LedgerRow) -> LedgerRow {
        LedgerRow {
            child_id: Some(ChildId::new(call_id, name)),
            parent_turn_id: Some(TurnId::from(turn)),
            ..row
        }
    }

    /// REQ-623 AC-12: a turn with two children reports `own + a + b` as its
    /// total and one line per child, each line carrying only that child's
    /// calls.
    ///
    /// The expected figures are literals, never read back from the report: the
    /// three amounts are distinct and non-zero so a child summed into the wrong
    /// line, or left out of the total, changes a number this test pins.
    ///
    /// Mutation (recorded): `total` accumulating only the turn's own rows
    /// reddens 3 tests — this one,
    /// `a_turn_is_keyed_by_session_and_needs_a_child_to_appear`, and
    /// `cost_attribution::parent_total_is_own_plus_children`. Dropping the
    /// per-child split (every child row onto the first line) reddens 2 — this
    /// one and `cost_attribution::parent_total_is_own_plus_children`; the
    /// one-child-per-turn `child_records_nest_under_parent_turn` cannot see it.
    #[test]
    fn a_turn_nests_each_child_and_totals_own_plus_children() {
        let prices = PriceTable::bundled();
        let rows = vec![
            in_turn(
                "turn-4",
                row("s1", None, "anthropic", "claude-fable-5", 0, 0, Some(1_000)),
            ),
            by_child(
                "call-9",
                "audit-a",
                "turn-4",
                row("s1", None, "deepseek", "deepseek-v4-pro", 10, 5, Some(20)),
            ),
            by_child(
                "call-9",
                "audit-b",
                "turn-4",
                row("s1", None, "deepseek", "deepseek-v4-pro", 30, 15, Some(300)),
            ),
            // audit-a's second call, after its sibling's: it lands on audit-a's
            // line, not on a third one.
            by_child(
                "call-9",
                "audit-a",
                "turn-4",
                row("s1", None, "anthropic", "claude-fable-5", 1, 1, Some(4)),
            ),
            in_turn(
                "turn-4",
                row(
                    "s1",
                    None,
                    "anthropic",
                    "claude-fable-5",
                    0,
                    0,
                    Some(50_000),
                ),
            ),
        ];
        let report = aggregate(&rows, &[], &prices);

        assert_eq!(report.per_turn.len(), 1, "one turn dispatched children");
        let turn = &report.per_turn[0];
        assert_eq!(turn.session_id, "s1");
        assert_eq!(turn.turn_id, TurnId::from("turn-4"));
        assert_eq!((turn.own.calls, turn.own.usd_micros), (2, 51_000));

        let lines: Vec<(&str, &str, u64, i64)> = turn
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.route.as_str(), c.calls, c.usd_micros))
            .collect();
        assert_eq!(
            lines,
            vec![
                (
                    "audit-a",
                    "deepseek/deepseek-v4-pro → anthropic/claude-fable-5",
                    2,
                    24
                ),
                ("audit-b", "deepseek/deepseek-v4-pro", 1, 300),
            ],
            "one line per child, in first-spend order, each with its own calls"
        );
        assert_eq!(
            turn.children[0].child_id,
            ChildId::new("call-9", "audit-a"),
            "the line keeps the full id a client matches on"
        );

        assert_eq!(turn.total.usd_micros, 51_000 + 24 + 300, "own + a + b");
        assert_eq!(turn.total.calls, 5);
        assert_eq!(turn.total.input_tokens, 41);
        // The view is not a second count: the session roll-up holds every row
        // exactly once.
        assert_eq!(report.total.usd_micros, 51_324);
        assert_eq!(report.per_session[0].calls, 5);
    }

    /// REQ-623: the ids add the nested view and move no figure in any roll-up
    /// that existed before them. The same calls with and without ids must
    /// report identical totals, savings, and per-session / per-phase /
    /// per-provider groups — and without ids, no per-turn entry at all.
    #[test]
    fn ids_add_a_nested_view_and_move_no_existing_roll_up() {
        let prices = PriceTable::bundled();
        let plain = vec![
            row(
                "s1",
                Some(Phase::Implement),
                "anthropic",
                "claude-fable-5",
                1000,
                500,
                prices.price("claude-fable-5", 1000, 500),
            ),
            row(
                "s1",
                Some(Phase::Implement),
                "deepseek",
                "deepseek-v4-pro",
                4000,
                2000,
                prices.price("deepseek-v4-pro", 4000, 2000),
            ),
        ];
        let stamped = vec![
            in_turn("turn-1", plain[0].clone()),
            by_child("call-1", "child-1", "turn-1", plain[1].clone()),
        ];

        let before = aggregate(&plain, &[], &prices);
        let after = aggregate(&stamped, &[], &prices);

        assert!(before.per_turn.is_empty(), "no ids, no nested view");
        assert_eq!(after.per_turn.len(), 1, "non-vacuity: the ids did nest");
        assert_eq!(before.total, after.total);
        assert_eq!(before.savings, after.savings);
        assert_eq!(before.unpriced, after.unpriced);
        assert_eq!(before.per_session, after.per_session);
        assert_eq!(before.per_phase, after.per_phase);
        assert_eq!(before.per_provider, after.per_provider);
        assert_eq!(before.probe_calls, after.probe_calls);
    }

    /// A turn id is minted off a per-process counter, so two sessions — or one
    /// session either side of a daemon restart sharing this file — can both
    /// hold a `turn-1`. The grouping keys on `(session, turn)`; and a turn whose
    /// own calls were stamped but which dispatched no child does not appear.
    #[test]
    fn a_turn_is_keyed_by_session_and_needs_a_child_to_appear() {
        let prices = PriceTable::bundled();
        let rows = vec![
            by_child(
                "call-1",
                "child-1",
                "turn-1",
                row("s1", None, "deepseek", "deepseek-v4-pro", 1, 1, Some(7)),
            ),
            by_child(
                "call-1",
                "child-1",
                "turn-1",
                row("s2", None, "deepseek", "deepseek-v4-pro", 1, 1, Some(11)),
            ),
            in_turn(
                "turn-2",
                row("s1", None, "anthropic", "claude-fable-5", 1, 1, Some(13)),
            ),
        ];
        let report = aggregate(&rows, &[], &prices);

        let turns: Vec<(&str, &str, i64)> = report
            .per_turn
            .iter()
            .map(|t| {
                (
                    t.session_id.as_str(),
                    t.turn_id.0.as_str(),
                    t.total.usd_micros,
                )
            })
            .collect();
        assert_eq!(turns, vec![("s1", "turn-1", 7), ("s2", "turn-1", 11)]);
    }

    #[test]
    fn methodology_names_the_baseline_and_flags_estimate() {
        let report = aggregate(&[], &[], &PriceTable::bundled());
        assert!(report.methodology.contains("Estimate"));
        assert!(report.methodology.contains("anthropic/claude-fable-5"));
        assert!(report.savings.is_estimate);
        // The savings payload carries the same methodology string.
        assert_eq!(report.methodology, report.savings.methodology);
    }
}
