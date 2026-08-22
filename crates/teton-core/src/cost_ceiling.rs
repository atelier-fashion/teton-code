//! The per-prompt spend ceiling's value types (REQ-588 ADR-1/2/5/7).
//!
//! Pure: an accumulator, a bound, and one sentence. The check that reads them
//! lives at the egress choke point (`tetond::egress`), which is the only place
//! that sees every remote call — but *what the refusal says* belongs here,
//! because the daemon raises it and the CLI renders it and a second composer
//! would drift (LESSON-529).
//!
//! # What the ceiling actually promises
//!
//! **A floor crossing, not a prediction** (ADR-2). A call's cost depends on its
//! *output* tokens, and nobody can price those before the model writes them. So
//! the rule is: refuse the next call once this prompt's recorded spend has
//! reached the ceiling. The consequence is that a prompt can overshoot by **at
//! most one call**, and that is said out loud — in the refusal, in `teton_docs`,
//! and in the release notes — rather than left for someone to discover from
//! their bill.
//!
//! The alternative, a pre-flight estimate from input tokens plus `max_tokens`,
//! was rejected: it makes the ceiling bind *earlier* than the user's number in
//! every case, which is a different lie.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What a prompt has spent so far, in integral micro-cents (ADR-1).
///
/// **Its lifetime is the prompt.** One is created where the prompt is and
/// threaded onto every call that prompt makes; when the prompt ends it is
/// dropped. That is what makes "per prompt" structural rather than a policy the
/// code has to remember — there is no key to get wrong, no map to prune, and no
/// missing-entry case to interpret.
///
/// Micro-cents, not dollars: the comparison that decides a refusal must be
/// exact, and a float accumulator would refuse or permit differently depending
/// on the order calls happened to complete in.
#[derive(Debug, Default)]
pub struct PromptSpend {
    micro_cents: AtomicU64,
    /// Whether any call this prompt made could not be priced.
    ///
    /// Sticky and separate from the total, because they are different facts: a
    /// prompt that spent $2 and *also* made one unpriceable call has a total
    /// that is a lower bound, not a measurement. Collapsing them would make the
    /// refusal claim precision it does not have.
    unpriced: AtomicBool,
}

impl PromptSpend {
    /// A fresh accumulator for one prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a completed call's cost.
    pub fn add(&self, micro_cents: u64) {
        self.micro_cents.fetch_add(micro_cents, Ordering::Relaxed);
    }

    /// Record that a call could not be priced (ADR-3).
    pub fn note_unpriced(&self) {
        self.unpriced.store(true, Ordering::Relaxed);
    }

    /// What this prompt has spent, in micro-cents.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.micro_cents.load(Ordering::Relaxed)
    }

    /// Whether any call this prompt made went unpriced.
    #[must_use]
    pub fn saw_unpriced(&self) -> bool {
        self.unpriced.load(Ordering::Relaxed)
    }

    /// ADR-2's predicate: has this prompt reached `ceiling`?
    ///
    /// `>=`, not `>`. A prompt that has spent exactly its ceiling has spent it;
    /// letting one more call through on the strength of an equality would make
    /// the ceiling mean "a bit more than this".
    #[must_use]
    pub fn reached(&self, ceiling_micro_cents: u64) -> bool {
        self.spent() >= ceiling_micro_cents
    }
}

/// Which ceiling bound a refusal (ADR-7, BR-2).
///
/// REQ-586's `BudgetBound` shape, applied to spend. There is **one** real
/// variant today, and that is the point rather than an oversight: adding a
/// second ceiling later is then a variant plus a rendering, not a retrofit of
/// "which number did we actually use" into a sentence that never had to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpendBound {
    /// `[cost] prompt_ceiling_usd` — the user's own per-prompt ceiling.
    PromptCeiling,
}

impl SpendBound {
    /// The words the bound is **said** in, as a lower-case fragment a caller
    /// may set in a sentence — `BudgetBound::words`' convention.
    ///
    /// Names the thing a user would go and change, which is why it is the key's
    /// name and not an abstract noun.
    #[must_use]
    pub const fn words(&self) -> &'static str {
        match self {
            SpendBound::PromptCeiling => "the per-prompt ceiling ([cost] prompt_ceiling_usd)",
        }
    }
}

/// A spend figure rendered as dollars, for a person.
///
/// Integral arithmetic in, a string out — the conversion happens once, here, at
/// the surface, and never on the path that decides a refusal.
#[must_use]
pub fn usd(micro_cents: u64) -> String {
    let cents = micro_cents / 1_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// **The one refusal sentence** (ADR-5, BR-2).
///
/// Composed here because the daemon raises it and the CLI renders it, and two
/// composers of "you hit your ceiling" drift the moment one of them gains a
/// figure (LESSON-529).
///
/// It names four things, and the fourth is the one most implementations would
/// omit: **that a call may have overshot**. A user reading a `$5.00` ceiling and
/// a `$5.12` spend deserves to know that is the design and not a bug — ADR-2's
/// bound made visible at the moment it matters.
#[must_use]
pub fn ceiling_refusal(spent: u64, ceiling: u64, bound: SpendBound) -> String {
    let overshot = spent > ceiling;
    format!(
        "this prompt has spent {} and reached {}, set to {}. It was refused before the next \
         call rather than continuing{}. The ceiling is checked between calls, so a call \
         already in flight can carry the total past it by its own cost.",
        usd(spent),
        bound.words(),
        usd(ceiling),
        if overshot {
            " — the last call completed past the line"
        } else {
            ""
        }
    )
}

/// The refusal when a call cannot be priced at all (ADR-3, OQ-2).
///
/// A *different* sentence, not a flag on the one above, because it is a
/// different problem with a different remedy: nothing was overspent, and the
/// user's move is to fix the price table (or drop the ceiling), not to raise a
/// number.
///
/// **Why this refuses rather than waves the call through.** An unpriced call
/// cannot be counted, so allowing it would make the ceiling silently
/// not-a-ceiling for exactly the provider nobody has a price for. A missing
/// price must not become a missing ceiling.
#[must_use]
pub fn unpriced_refusal(provider_id: &str, model: &str, bound: SpendBound) -> String {
    format!(
        "{} is set, but this call to `{provider_id}` (model `{model}`) cannot be priced, so it \
         cannot be counted against the ceiling. Refused rather than sent uncounted — a missing \
         price must not become a missing ceiling. Add a price for this model, or remove the \
         ceiling to send it unmetered.",
        // Capitalised at the head of a sentence; the fragment is lower-case by
        // convention so it can also sit mid-sentence above.
        {
            let w = bound.words();
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accumulator_adds_and_reads_back() {
        let spend = PromptSpend::new();
        assert_eq!(spend.spent(), 0);
        assert!(!spend.saw_unpriced());

        spend.add(1_500);
        spend.add(2_500);
        assert_eq!(spend.spent(), 4_000);
    }

    /// **ADR-2.** `reached` is `>=`, and it is a floor crossing rather than a
    /// prediction.
    #[test]
    fn reached_is_inclusive_because_spending_exactly_the_ceiling_is_spending_it() {
        let spend = PromptSpend::new();
        spend.add(499_999);
        assert!(!spend.reached(500_000), "a hair under is under");

        spend.add(1);
        assert!(
            spend.reached(500_000),
            "exactly the ceiling has reached it — letting one more call through \
             on an equality would make the ceiling mean 'a bit more than this'"
        );

        spend.add(10_000);
        assert!(spend.reached(500_000), "and past it stays reached");
    }

    /// The unpriced flag is sticky and **separate** from the total.
    #[test]
    fn unpriced_is_a_separate_sticky_fact() {
        let spend = PromptSpend::new();
        spend.add(1_000);
        spend.note_unpriced();
        spend.add(1_000);

        assert_eq!(
            spend.spent(),
            2_000,
            "the total is unaffected — it is a lower bound now, not a wrong number"
        );
        assert!(spend.saw_unpriced(), "and the flag stays set");
    }

    /// The accumulator is shared across threads, because a prompt's calls are.
    #[test]
    fn the_accumulator_is_shared_across_threads() {
        use std::sync::Arc;
        let spend = Arc::new(PromptSpend::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&spend);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        s.add(10);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(spend.spent(), 8 * 100 * 10);
    }

    #[test]
    fn dollars_render_from_integral_micro_cents() {
        assert_eq!(usd(0), "$0.00");
        assert_eq!(usd(1_000), "$0.01");
        assert_eq!(usd(500_000), "$5.00");
        assert_eq!(usd(512_340), "$5.12");
        // Sub-cent remainders truncate in the *display* only; the comparison
        // never sees this function.
        assert_eq!(usd(999), "$0.00");
    }

    /// **ADR-5 / BR-2.** The refusal names spend, bound, ceiling — and the
    /// overshoot, which is the part most implementations would omit.
    #[test]
    fn the_refusal_names_the_spend_the_bound_and_the_overshoot() {
        // Exactly at the line: no overshoot clause, because nothing overshot.
        let exact = ceiling_refusal(500_000, 500_000, SpendBound::PromptCeiling);
        assert!(exact.contains("$5.00"), "{exact}");
        assert!(exact.contains("prompt_ceiling_usd"), "{exact}");
        assert!(
            !exact.contains("completed past the line"),
            "nothing overshot, so the clause must not appear: {exact}"
        );
        assert!(
            exact.contains("a call already in flight can carry the total past it"),
            "the standing caveat is always said — it is what the ceiling actually \
             promises: {exact}"
        );

        // Past the line: the clause appears, and the figures are the real ones.
        let over = ceiling_refusal(512_340, 500_000, SpendBound::PromptCeiling);
        assert!(over.contains("$5.12") && over.contains("$5.00"), "{over}");
        assert!(
            over.contains("the last call completed past the line"),
            "a user reading a $5.00 ceiling and a $5.12 spend must be told that \
             is the design and not a bug: {over}"
        );
    }

    /// **ADR-3 / OQ-2.** The unpriced refusal is a different sentence with a
    /// different remedy.
    #[test]
    fn the_unpriced_refusal_names_the_provider_and_the_remedy() {
        let line = unpriced_refusal("moonshot", "kimi-k3", SpendBound::PromptCeiling);
        assert!(
            line.contains("moonshot") && line.contains("kimi-k3"),
            "{line}"
        );
        assert!(
            line.contains("a missing price must not become a missing ceiling"),
            "the reason is part of the sentence, because 'refused' without it \
             reads as a bug: {line}"
        );
        assert!(
            line.contains("Add a price") && line.contains("remove the ceiling"),
            "both remedies, since the user may legitimately want either: {line}"
        );
        assert!(
            line.starts_with("The per-prompt ceiling"),
            "capitalised at the head of a sentence: {line}"
        );
        // It is NOT the overspend sentence — different problem, different fix.
        assert!(!line.contains("has spent"), "{line}");
    }

    /// **ADR-1, as a fact about the code.** This module does no I/O and no
    /// float arithmetic.
    ///
    /// Fails **open** on a read error rather than panicking (BUG-159).
    #[test]
    fn this_module_is_pure_and_integral() {
        let Ok(src) =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cost_ceiling.rs"))
        else {
            return;
        };
        let body = src.split("#[cfg(test)]").next().unwrap_or(&src);
        for forbidden in ["std::fs", "f64", "f32", "read_dir"] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` appears in the pure half — the comparison that \
                 decides a refusal must be integral, and the daemon owns the I/O"
            );
        }
    }
}
