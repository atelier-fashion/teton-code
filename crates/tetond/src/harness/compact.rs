//! The `compact` duty: deciding what a conversation may forget, ahead of the
//! hard budget gate (REQ-561 BR-1/BR-2/BR-3/BR-4/BR-4a/BR-7/BR-8, TASK-063).
//!
//! ## Not a tool's duty — the context's
//!
//! `triage` and `shell` hang off the tool that owns them, through
//! [`Tool::refine`](super::tools::Tool::refine) (ADR-10); `title` hangs off
//! session creation. `compact` hangs off
//! [`ContextManager::compact_if_pressured`](super::context::ContextManager::compact_if_pressured),
//! because the thing that knows the conversation no longer fits is the thing
//! holding the conversation.
//!
//! ## It runs *ahead of* the hard gate, never *instead of* it (ADR-4)
//!
//! `truncate_to_budget()` still fires unconditionally afterward, unmodified and
//! unconditional. That is what makes BR-4 **structural**: a duty that hangs,
//! returns garbage, returns an over-budget answer, is never routed, or panics
//! cannot produce an over-budget context, because the thing enforcing the budget
//! was never the duty. Everything in this module is therefore an *improvement* on
//! which blocks are dropped — never the reason the budget holds.
//!
//! The corollary is the soft threshold ([`COMPACT_PRESSURE_PERCENT`]): compaction
//! runs with headroom rather than at the exact moment the window is already full.
//! A threshold within a rounding error of 100% would defeat the whole decision —
//! the duty would fire only in the emergency it exists to pre-empt.
//!
//! ## The answer is numbers **and** the paragraph that stands in for them
//!
//! A duty that only named blocks to forget would throw their content away; a duty
//! that only wrote prose could rewrite history wholesale. So the contract asks for
//! both: the blocks to drop, and one paragraph replacing them. The numbers are
//! resolved against the list this module offered, so a number invented out of
//! range has nowhere to land and the survivors are always a subset of the blocks
//! that were really there. Only the replacement paragraph is new text, and it
//! enters context under the same control-token cut an agent turn gets.
//!
//! This is also why [`COMPACT_OUTPUT_MAX_BYTES`] is the **loosest** of the five
//! ceilings: a `title` is a handful of words and a `triage` is a list of numbers,
//! but a compaction stands in for a conversation (BR-8).
//!
//! ## Strict where its siblings are lenient — and that is BR-4
//!
//! [`read_ranking`](super::triage) ignores whatever it cannot read, because a
//! ranking that drops a junk token is still a ranking. [`read_compaction`] does
//! the opposite: **any** unreadable part fails the whole answer. A compaction
//! applied in part is the worst outcome available — it corrupts the context *and*
//! leaves the budget unmet — so there is no half-parsed answer to salvage, and
//! this parser never returns one. The apply step in
//! [`super::context`] closes the other half: the surviving conversation is built
//! **entirely** as a candidate and committed with a single assignment, so partial
//! application is impossible by construction rather than by care.
//!
//! ## What the duty prompt's framing is, and is not
//!
//! The numbered block list is *advisory*: it is how the duty is asked the
//! question, and content that forges a block header can at worst make the duty
//! choose badly. Nothing about the applied decision is derived from block text —
//! the range check, the protected most-recent block, the budget check and the
//! atomic commit all hold whatever the prompt looked like.
//!
//! (On writing about the resolver in [`crate::runtime`]: describe it, never spell
//! it. The `declared, no call site yet` marker in [`crate::call_sites`] is derived
//! by scanning the daemon's source as **text**, so the spelling of a
//! category-resolving router call inside a doc comment registers as a call site
//! and turns the derived-marker test red. ADR-9, learned the hard way in
//! TASK-058.)
//!
//! # What breaks which test
//!
//! The chain is **duty → parse → apply → `truncate_to_budget`**, and each link
//! needs its own mutation: a test that only mutates the outer link leaves the
//! inner fallback unverified (LESSON-483). Every row below was **applied and
//! observed failing**, not reasoned about (LESSON-441); none came back green.
//! Test paths are relative to `tetond`'s lib target.
//!
//! | Mutation | Fails |
//! |---|---|
//! | **link 1** — `compact_if_pressured` returns declined before ever asking the duty | `harness::context::tests::a_routed_compaction_replaces_the_blocks_it_forgets`, `…::compaction_runs_ahead_of_the_hard_gate_not_at_it`, `runtime::tests::dispatch::compact::a_performed_compaction_announces_its_route_and_a_declined_one_does_not`, and every other compaction test in `harness::context` |
//! | **link 2** (parse) — `read_compaction` keeps the numbers it managed to read instead of failing the whole answer | `harness::compact::tests::a_partly_readable_answer_is_no_answer_at_all`, `harness::context::tests::a_half_readable_compaction_is_not_half_applied` |
//! | **link 3** (apply) — the over-budget candidate is committed instead of rejected | `harness::context::tests::an_over_budget_compaction_is_rejected_rather_than_rescued` |
//! | **link 3** (apply) — the no-shrink refusal is removed | `harness::context::tests::a_compaction_that_does_not_shrink_the_context_is_rejected` |
//! | **link 3** (apply) — the forget set is applied but no replacement block is inserted | `harness::context::tests::a_routed_compaction_replaces_the_blocks_it_forgets` + 4 others |
//! | **link 4** — the loop's unconditional `truncate_to_budget()` is made conditional on the compaction having worked | `harness::turn_loop::tests::a_turn_whose_compact_duty_cannot_serve_still_ends_under_budget` — **and nothing else in the suite**, which is exactly why that test exists at the loop rather than at the manager |
//! | the soft threshold becomes the hard gate's own (compaction only at the emergency) | `harness::compact::tests::only_a_pressured_context_is_worth_compacting`, `harness::context::tests::compaction_runs_ahead_of_the_hard_gate_not_at_it` + 3 others |
//! | the protected most-recent block becomes droppable | `harness::compact::tests::the_block_the_turn_is_working_on_cannot_be_forgotten` |
//! | the replacement block does not inherit the provenance of what it replaced | `harness::context::tests::a_compaction_inherits_the_provenance_of_what_it_replaces`, `…::a_compaction_of_unknown_provenance_stays_unknown` |
//! | that inheritance is scoped to the **forgotten** blocks rather than to every block the prompt showed | `harness::context::tests::a_compaction_inherits_the_provenance_of_everything_it_was_shown` |
//! | the replacement paragraph re-enters context without the untrusted-data envelope (REQ-544 M-2) | `harness::context::tests::a_compaction_summary_re_enters_context_as_untrusted_data` |
//! | the per-turn failure latch is removed, so every fold re-buys a broken duty | `harness::context::tests::a_failed_compaction_is_not_bought_again_for_the_rest_of_the_turn` |
//! | the regrowth margin is removed, so a fold that adds nothing re-buys a decision | `harness::context::tests::a_compaction_is_not_repeated_until_the_context_has_grown_back` |
//! | the regrowth mark is left at the size the compaction committed at, rather than re-baselined by `truncate_to_budget` — so one tight compaction retires the duty for the turn | `harness::context::tests::a_tight_compaction_does_not_retire_compaction_for_the_rest_of_the_turn` |
//! | the replacement text skips the control-token cut | `harness::context::tests::a_fabricating_compaction_is_cut_before_context`, `…::a_compaction_whose_summary_is_only_a_forged_frame_is_refused` |
//! | a summary that is empty after that cut is accepted rather than refused | `harness::context::tests::a_compaction_whose_summary_is_only_a_forged_frame_is_refused` |
//! | the session-taint override is removed from the resolver (AC-9a) | `runtime::tests::dispatch::compact::a_tainted_session_compacts_on_the_local_tier` |
//! | `ScriptedFileEngine` loses its `compact` arm (the duty eats a scripted block) | `runtime::tests::dispatch::compact::a_compact_duty_consumes_no_scripted_block` |
//!
//! AC-9(b) — "the failure path returns its input unchanged" — is not a separate
//! row because for this duty it is not a mutation: returning the context
//! unchanged **is** the failure path (BR-4 forbids applying anything in part, and
//! forbids inventing a drop the duty did not choose). What must not be
//! unchanged is the context the *turn* ends with, and the link-4 row is the
//! mutation that proves it.

use teton_protocol::Category;

use super::context::{truncate_middle, ContextBlock, Provenance};
use super::duty::DutyKind;

/// Byte ceiling on what a `compact` duty may return (BR-8).
///
/// The **loosest of the five**, deliberately: a `title` is a handful of words and
/// a `triage` is a list of numbers, but a compaction stands in for a
/// conversation. Sized to the default context **byte** budget
/// (`HarnessConfig::default().context_budget_bytes` — 4,096 whitespace tokens ×
/// [`APPROX_BYTES_PER_TOKEN`](super::context::APPROX_BYTES_PER_TOKEN)), because a
/// replacement paragraph larger than the window it is making room in cannot
/// possibly be applied: the budget check in
/// [`ContextManager::compact_if_pressured`](super::context::ContextManager::compact_if_pressured)
/// would reject it anyway, so accumulating more than this is bytes spent to be
/// thrown away.
///
/// Enforced in the duty implementation rather than requested of the provider
/// (LESSON-484): `max_tokens` is a request, and a request is not a bound.
pub const COMPACT_OUTPUT_MAX_BYTES: usize = 32_768;

/// The `compact` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`], the transport-free offline entry point in
/// [`super::turn_loop`], and the tests.
pub const COMPACT_DUTY: DutyKind = DutyKind::new(Category::Compact, COMPACT_OUTPUT_MAX_BYTES);

/// The compact duty's output contract, verbatim: the last sentence of the
/// instruction, before the numbered blocks it embeds.
///
/// Exported because it is also how the CI/offline stand-in engine
/// ([`crate::runtime::ScriptedFileEngine`]) recognizes a `compact` duty and
/// answers it *without consuming a scripted turn* — **a duty is not a turn**
/// (BR-10). One constant, used both to write the sentence and to recognize it, so
/// the seam cannot drift out of step with the prompt. A duty with no recognition
/// arm eats a scripted block and shifts every fixture's turn sequence by one,
/// which REQ-558 shipped twice before it was caught.
///
/// A full, distinctive sentence rather than a short phrase, for the reason
/// [`SUMMARIZER_OUTPUT_CONTRACT`](super::context::SUMMARIZER_OUTPUT_CONTRACT) is
/// one: the recognizer sees the *whole* rendered prompt, and this duty's prompt
/// embeds a whole conversation — so a generic phrase could plausibly arrive
/// inside the very conversation being compacted.
pub const COMPACT_OUTPUT_CONTRACT: &str = "Reply with two lines and nothing else: `FORGET:` \
     followed by the numbers of the blocks to drop, then `SUMMARY:` followed by the one \
     paragraph that stands in for them.";

/// Fraction of the context budget at which compaction becomes worth a model call
/// — the **soft** threshold (BR-4a, OQ-3 resolved, ADR-11).
///
/// Deliberately well clear of 100%. The whole point of resolving OQ-3 the way it
/// was resolved is that compaction runs *before* the emergency: at 95% the duty
/// would fire only when the window is already full, which is both the moment a
/// turn can least afford to wait for another model call and the moment there is
/// least room to put a replacement paragraph. 70% leaves roughly a third of the
/// window as working room.
///
/// A named constant with a zero-call test rather than an inline literal, because
/// a hidden threshold is a cost surprise in one direction and a corruption
/// surprise in the other (ADR-11).
pub const COMPACT_PRESSURE_PERCENT: usize = 70;

/// Fewest conversation blocks worth spending a `compact` model call on (ADR-11).
///
/// Two blocks offer exactly one droppable block, and "drop the only thing you may
/// drop" is not a decision a model is needed for —
/// [`truncate_to_budget`](super::context::ContextManager::truncate_to_budget)
/// already makes it, for free and deterministically. Below this the duty declines
/// and nothing is spent.
///
/// The negative case is the cost argument: a session whose context is two blocks
/// long pays nothing however hard it is pressing on its budget.
pub const COMPACT_MIN_BLOCKS: usize = 3;

/// How much the context must have **grown** since the last applied compaction
/// before another one is worth buying, as a percentage of the byte budget
/// (ADR-11).
///
/// The soft threshold alone is a re-entry condition, not a rate limit. A
/// successful compaction only has to land under 100% — nothing makes it land
/// under [`COMPACT_PRESSURE_PERCENT`] — so a long turn that stays pressured
/// buys one `compact` model call per **tool result**, each one asking a model to
/// re-decide a conversation that has grown by one fold. This is the shape
/// `SessionRegistry::claim_title` exists to avoid for `title`, and the shape
/// `TRIAGE_MIN_MATCHES` and BR-4b's resolved trigger avoid for their duties.
///
/// A named constant with its own zero-call test rather than an inline literal,
/// because a hidden threshold is a cost surprise (ADR-11).
pub const COMPACT_REGROWTH_PERCENT: usize = 10;

/// Whether a context already compacted at `committed` bytes has grown enough to
/// be worth compacting again (ADR-11).
///
/// A zero budget always says yes, matching [`under_pressure`]'s answer for the
/// same input: a context with no room at all is never spared a decision.
#[must_use]
pub const fn worth_compacting_again(estimated: usize, committed: usize, budget: usize) -> bool {
    estimated >= committed.saturating_add(budget.saturating_mul(COMPACT_REGROWTH_PERCENT) / 100)
}

/// Display bound on one block's text inside the duty prompt, in bytes.
///
/// Only the *display* is bounded: the answer is block numbers, and a number
/// resolves to the whole original block. Head-and-tail ([`truncate_middle`])
/// rather than a head cut, because what a block was about is legible from its
/// opening and its ending — a 200 KB paste's first kilobyte is all boilerplate.
const COMPACT_BLOCK_MAX_BYTES: usize = 1_024;

/// The header that opens the numbered block list in a compact prompt.
///
/// Written once and read once ([`offered_block_count`]), for the same reason the
/// output contract is: two places describing one prompt shape must not be able to
/// drift.
const BLOCK_LIST_HEADER: &str = "\nConversation:\n";

/// The marker introducing the blocks to forget.
const FORGET_MARKER: &str = "FORGET:";

/// The marker introducing the paragraph that replaces them.
const SUMMARY_MARKER: &str = "SUMMARY:";

/// One compaction the duty asked for, already checked against the list it was
/// offered.
///
/// Fields are read-only to the outside world for a reason: the only way to obtain
/// one is [`read_compaction`], so a `Compaction` in hand is always a *whole*
/// answer that parsed — there is no way to construct the half-parsed value BR-4
/// forbids applying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    forget: Vec<usize>,
    summary: String,
}

impl Compaction {
    /// **Zero-based** indices of the blocks to forget, ascending and deduplicated.
    ///
    /// Zero-based because they index the caller's block slice directly; the
    /// prompt numbers from 1 because that is what a model reliably reads, and
    /// [`read_compaction`] is the one place the two numberings meet.
    #[must_use]
    pub fn forget(&self) -> &[usize] {
        &self.forget
    }

    /// The paragraph that stands in for the forgotten blocks. Never empty.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

/// Whether `estimated` has crossed the soft threshold on `budget` (BR-4a).
///
/// A zero budget is *always* under pressure, which is the honest answer: a
/// context with no room at all is the most pressured context there is.
#[must_use]
pub const fn under_pressure(estimated: usize, budget: usize) -> bool {
    estimated > budget.saturating_mul(COMPACT_PRESSURE_PERCENT) / 100
}

/// Whether a conversation of `blocks` blocks is worth a `compact` model call
/// (ADR-11).
#[must_use]
pub const fn worth_compacting(blocks: usize) -> bool {
    blocks >= COMPACT_MIN_BLOCKS
}

/// The duty prompt: what to produce, and the numbered conversation to produce it
/// from.
///
/// Blocks are numbered from 1 and the **last** one is marked as un-droppable —
/// it is the step the turn is working on, and it is the one block
/// [`truncate_to_budget`](super::context::ContextManager::truncate_to_budget)
/// also refuses to drop. Marking it in the prompt saves the duty from spending
/// its answer on a block that would be rejected; [`read_compaction`] is what
/// makes the refusal real.
#[must_use]
pub fn compact_prompt(blocks: &[ContextBlock]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Below is a numbered list of the blocks of one conversation between a person and an \
         AI coding agent. It no longer fits the agent's context window. Choose the blocks \
         whose content is no longer needed to carry on the work, and write the one paragraph \
         that preserves what a reader would still need from them. ",
    );
    prompt.push_str(COMPACT_OUTPUT_CONTRACT);
    prompt.push_str(BLOCK_LIST_HEADER);
    let last = blocks.len();
    for (i, block) in blocks.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. {}: {}\n",
            i + 1,
            speaker(block),
            truncate_middle(&block.text, COMPACT_BLOCK_MAX_BYTES)
        ));
        if i + 1 == last {
            prompt.push_str("   (the step in progress — this block cannot be forgotten)\n");
        }
    }
    prompt
}

/// How many blocks a compact prompt offered.
///
/// The stand-in engine's way to answer a `compact` duty with a decision that fits
/// the conversation it was actually shown (BR-10), rather than one that happens
/// to be in range. Reads the list the prompt carries — after rendering, and after
/// the last header — so a prompt edit that changed the numbering shows up here
/// rather than as an unusable answer two fixtures later.
#[must_use]
pub fn offered_block_count(prompt: &str) -> usize {
    let Some(at) = prompt.rfind(BLOCK_LIST_HEADER) else {
        return 0;
    };
    let mut offered = 0usize;
    for line in prompt[at + BLOCK_LIST_HEADER.len()..].lines() {
        if line.starts_with(&format!("{}. ", offered + 1)) {
            offered += 1;
        }
    }
    offered
}

/// Read a compaction out of `answer`, or fail the **whole** answer (BR-4).
///
/// `droppable` is how many of the offered blocks may be forgotten — the leading
/// `droppable` of them, since the most recent block is the step in progress. A
/// number naming it, or naming nothing that was offered, is not a number to be
/// skipped: it means the answer was not written against the list this duty
/// offered, and applying the rest of it would be applying a decision made about a
/// different conversation.
///
/// This is where `compact` deliberately parts company with its siblings.
/// `triage`'s reader ignores whatever it cannot use, because a ranking missing a
/// junk token is still a ranking. Here, a compaction applied in part corrupts the
/// context *and* leaves the budget unmet, so there is nothing to salvage: the
/// answer is whole or it is nothing.
///
/// # Errors
/// A sentence naming what was wrong with the answer — no forget marker, no
/// summary marker, prose where the numbers belong, a number naming a block that
/// may not be forgotten, an empty forget set, or an empty summary. Every one of
/// them leaves the caller holding an explanation it can degrade with
/// (LESSON-447).
pub fn read_compaction(answer: &str, droppable: usize) -> Result<Compaction, String> {
    let Some(forget_at) = answer.find(FORGET_MARKER) else {
        return Err(
            "the `compact` duty's answer named no blocks to forget (no `FORGET:` line)".to_owned(),
        );
    };
    let after_forget = &answer[forget_at + FORGET_MARKER.len()..];
    let Some(summary_at) = after_forget.find(SUMMARY_MARKER) else {
        return Err(
            "the `compact` duty's answer carried no replacement summary (no `SUMMARY:` line)"
                .to_owned(),
        );
    };
    let numbers = &after_forget[..summary_at];
    let summary = after_forget[summary_at + SUMMARY_MARKER.len()..].trim();

    // Strict on purpose: prose between the two markers means the answer is not
    // the two lines that were asked for, and reading the digits out of it would
    // be guessing which of them were block numbers.
    if let Some(bad) = numbers
        .chars()
        .find(|c| !c.is_ascii_digit() && *c != ',' && !c.is_whitespace())
    {
        return Err(format!(
            "the `compact` duty's answer put {bad:?} where the block numbers belong, so the \
             whole answer was discarded"
        ));
    }

    let mut forget = Vec::new();
    let mut taken = vec![false; droppable];
    for token in numbers
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
    {
        // An absurdly long digit run overflows `usize` and parses to `Err`. It is
        // rejected rather than skipped: it is a number the duty meant, and this
        // parser does not silently drop numbers.
        let Ok(n) = token.parse::<usize>() else {
            return Err(format!(
                "the `compact` duty's answer named block {token}, which was never offered"
            ));
        };
        if n == 0 || n > droppable {
            return Err(format!(
                "the `compact` duty's answer named block {n}, which it may not forget \
                 (blocks 1..={droppable} were offered)"
            ));
        }
        if !taken[n - 1] {
            taken[n - 1] = true;
            forget.push(n - 1);
        }
    }
    if forget.is_empty() {
        // "Forget nothing" is the keep-everything answer, and keep-everything
        // breaks the budget by a different route (BR-4). The caller degrades to
        // deterministic truncation instead, which at least drops something.
        return Err("the `compact` duty's answer named no blocks to forget".to_owned());
    }
    if summary.is_empty() {
        // Dropping blocks and replacing them with nothing is a compaction that
        // forgets without remembering — worse than the deterministic drop it was
        // meant to improve on.
        return Err("the `compact` duty's answer carried an empty replacement summary".to_owned());
    }
    forget.sort_unstable();
    Ok(Compaction {
        forget,
        summary: summary.to_owned(),
    })
}

/// How a block is introduced in the duty prompt — its role, plus the tool that
/// produced it when there was one, exactly as the transcript rendering names it.
fn speaker(block: &ContextBlock) -> String {
    match &block.provenance {
        Provenance::Tool { tool, .. } => format!("{} ({tool})", block.role.label()),
        _ => block.role.label().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;

    use std::sync::{Arc, Mutex};

    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_inference::{Engine, MockEngine};

    use crate::egress::Provenance as EgressProvenance;
    use crate::harness::context::{BlockRole, ContextManager, ToolProvenance};
    use crate::harness::duty::testing::{remote_duty_route, wire, Sent};
    use crate::harness::duty::DutyRoute;

    fn conversation() -> Vec<ContextBlock> {
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_user("port the download client to the new retry API");
        ctx.push_model("{\"tool\":\"read\"}");
        ctx.push_tool_result("read", Some(fixture_id("src/download.rs")), "fn get() {}");
        ctx.push_model("I will edit it now");
        ctx.blocks().to_vec()
    }

    // -- the decline thresholds (ADR-11, BR-4a) ------------------------------

    /// **The soft threshold, stated once**, and the load-bearing half is the one
    /// that costs nothing: a context with room to spare buys no model call.
    ///
    /// Read off [`COMPACT_PRESSURE_PERCENT`] rather than written as literals, so
    /// moving the threshold moves the test with it instead of silently
    /// un-testing it.
    #[test]
    fn only_a_pressured_context_is_worth_compacting() {
        let budget = 1_000;
        let at = budget * COMPACT_PRESSURE_PERCENT / 100;
        assert!(!under_pressure(at, budget), "exactly at the threshold");
        assert!(!under_pressure(at - 1, budget));
        assert!(under_pressure(at + 1, budget));
        // The headroom is the whole point of OQ-3's resolution: the duty must
        // fire well before the window is full, not at the emergency.
        assert!(
            under_pressure(budget * 4 / 5, budget),
            "four-fifths of the budget must count as pressure"
        );
        // A compile-time claim, because it is a claim about a constant: raising
        // the threshold into the emergency stops the build rather than waiting
        // for a test run.
        const {
            assert!(
                COMPACT_PRESSURE_PERCENT <= 80,
                "a threshold within a rounding error of 100% defeats the decision"
            );
        }
        // A zero budget is the most pressured context there is, not the least.
        assert!(under_pressure(1, 0));
    }

    /// The other decline: a conversation too short to hold a decision.
    #[test]
    fn only_a_conversation_with_a_choice_in_it_is_worth_compacting() {
        assert!(!worth_compacting(0));
        assert!(!worth_compacting(COMPACT_MIN_BLOCKS - 1));
        assert!(worth_compacting(COMPACT_MIN_BLOCKS));
        const {
            assert!(
                COMPACT_MIN_BLOCKS >= 3,
                "two blocks offer exactly one droppable block, which is not a decision"
            );
        }
    }

    // -- the prompt ----------------------------------------------------------

    /// The output contract is the *whole* of the sentence the stand-in engine
    /// recognizes, and it really is in the prompt.
    ///
    /// Written out here rather than reused from the constant for the reason its
    /// siblings' equivalents are: changing it must be a deliberate two-place edit
    /// rather than something that silently desynchronizes `ScriptedFileEngine`
    /// from the duty it is meant to answer off-script (BR-10).
    #[test]
    fn the_duty_prompt_carries_the_output_contract_verbatim() {
        assert_eq!(
            COMPACT_OUTPUT_CONTRACT,
            "Reply with two lines and nothing else: `FORGET:` followed by the numbers of the \
             blocks to drop, then `SUMMARY:` followed by the one paragraph that stands in for \
             them."
        );
        assert!(compact_prompt(&conversation()).contains(COMPACT_OUTPUT_CONTRACT));
    }

    /// The prompt numbers every block from 1, names its speaker, and says which
    /// one may not be forgotten — the three things the answer is written against.
    #[test]
    fn the_duty_prompt_numbers_every_block_and_protects_the_last() {
        let blocks = conversation();
        let prompt = compact_prompt(&blocks);
        assert_eq!(offered_block_count(&prompt), blocks.len());
        assert!(prompt.contains("1. User: port the download client"));
        assert!(prompt.contains("3. Tool (read): fn get() {}"));
        assert!(prompt.contains("4. Assistant: I will edit it now"));
        assert!(prompt.contains("cannot be forgotten"));
    }

    /// A pasted core dump cannot make the prompt unbounded: every block is
    /// display-bounded in **bytes**, the unit the wire is measured in.
    #[test]
    fn an_enormous_block_is_bounded_in_the_prompt() {
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_user("é".repeat(50_000));
        ctx.push_model("ok");
        ctx.push_user("and now?");
        let prompt = compact_prompt(ctx.blocks());
        assert!(
            prompt.len() < 3 * COMPACT_BLOCK_MAX_BYTES + 1_024,
            "a 100 KB block produced a {} byte prompt",
            prompt.len()
        );
        assert!(prompt.contains(COMPACT_OUTPUT_CONTRACT));
        assert_eq!(offered_block_count(&prompt), 3);
    }

    // -- the parser: whole answers only (BR-4) -------------------------------

    #[test]
    fn a_well_formed_answer_reads_back_as_the_blocks_and_the_paragraph() {
        let c = read_compaction("FORGET: 1, 3\nSUMMARY: they read one file.", 3)
            .expect("a well-formed answer");
        assert_eq!(c.forget(), [0, 2]);
        assert_eq!(c.summary(), "they read one file.");
    }

    /// Order and repetition in the answer do not survive into the decision: the
    /// forget set is ascending and deduplicated, so an answer of `3 3 3 1` can
    /// never drop more blocks than it named.
    #[test]
    fn a_repeated_or_unordered_answer_still_names_one_set_of_blocks() {
        let c = read_compaction("FORGET: 3 3 1 3\nSUMMARY: x", 3).expect("readable");
        assert_eq!(c.forget(), [0, 2]);
    }

    /// **BR-4's parser half.** An answer that is readable in part is not read in
    /// part — the blocks it *did* name are not dropped.
    #[test]
    fn a_partly_readable_answer_is_no_answer_at_all() {
        for (answer, why) in [
            (
                "FORGET: 1, 2, banana\nSUMMARY: x",
                "prose among the numbers",
            ),
            ("FORGET: 1 2\nthey read a file", "no summary marker"),
            ("blocks 1 and 2 can go\nSUMMARY: x", "no forget marker"),
            ("FORGET: 1 2\nSUMMARY:   ", "an empty summary"),
            ("FORGET:\nSUMMARY: x", "no blocks named"),
            ("FORGET: none\nSUMMARY: x", "a word where a number belongs"),
        ] {
            let err =
                read_compaction(answer, 3).expect_err(&format!("{why} must fail the whole answer"));
            assert!(!err.is_empty(), "{why} must be explained");
        }
    }

    /// **The protected block.** The step the turn is working on is not offered as
    /// droppable, and naming it fails the whole answer rather than being skipped
    /// — an answer written about a block that may not move was not written about
    /// this list.
    #[test]
    fn the_block_the_turn_is_working_on_cannot_be_forgotten() {
        // Four blocks offered, three droppable: 4 is the step in progress.
        let err = read_compaction("FORGET: 1 4\nSUMMARY: x", 3).expect_err("4 is protected");
        assert!(err.contains("may not forget"), "{err}");
        // And it really is only the last one that is refused.
        assert!(read_compaction("FORGET: 1 3\nSUMMARY: x", 3).is_ok());
    }

    /// A number nobody offered, and a number too large to be one, are both
    /// answers about a different conversation.
    #[test]
    fn a_number_that_was_never_offered_fails_the_whole_answer() {
        assert!(read_compaction("FORGET: 0\nSUMMARY: x", 3).is_err());
        assert!(read_compaction("FORGET: 99\nSUMMARY: x", 3).is_err());
        let err = read_compaction(&format!("FORGET: {}\nSUMMARY: x", "9".repeat(64)), 3)
            .expect_err("an overflowing number is still a number the duty meant");
        assert!(err.contains("never offered"), "{err}");
    }

    // -- the route legs ------------------------------------------------------

    fn local_route(reply: &str) -> DutyRoute {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", reply)));
        DutyRoute::local(COMPACT_DUTY, "local", engine)
    }

    /// **The generation budget is this duty's, not one shared with the other
    /// four.** A compaction stands in for a conversation — that is the whole
    /// argument for [`COMPACT_OUTPUT_MAX_BYTES`] being the loosest of the five —
    /// so a duty asking for the same handful of tokens a `title` asks for has a
    /// ceiling that describes nothing it can actually produce.
    ///
    /// The marker at the end of the fixture is what makes this discriminating:
    /// the answer is well inside the byte ceiling and well outside the shared
    /// default's token budget, so a summary cut short is a summary the tail of
    /// which never arrives.
    #[tokio::test]
    async fn a_local_compaction_may_write_the_paragraph_its_ceiling_allows() {
        const TAIL: &str = "END-OF-SUMMARY";
        let long = format!("FORGET: 1 2\nSUMMARY: {}{TAIL}", "word ".repeat(900));
        assert!(
            long.len() < COMPACT_OUTPUT_MAX_BYTES,
            "non-vacuity: the fixture must fit the ceiling, or this tests the ceiling"
        );

        let route = local_route(&long);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve");
        };
        let answer = duty
            .perform("compact this", &EgressProvenance::empty())
            .await
            .expect("the local duty served");

        assert!(
            answer.ends_with(TAIL),
            "the compaction was cut {} bytes short of the paragraph it wrote — a \
             generation budget shared with `title` is not this duty's ceiling",
            long.len() - answer.len()
        );
        assert_eq!(
            read_compaction(&answer, 3)
                .expect("a whole answer parses")
                .forget(),
            &[0, 1]
        );
    }

    /// The ceiling is the loosest of the five, and it is loosest **by
    /// construction** rather than by coincidence.
    ///
    /// The neighbour comparisons are compile-time because the relationship
    /// between two constants is a compile-time fact: lowering this ceiling below
    /// a neighbour's stops the build rather than waiting for a test run.
    /// `digest`'s own ceiling is private, so it is compared through the constant
    /// it is defined as.
    #[test]
    fn the_compact_ceiling_is_the_loosest_of_the_five() {
        const {
            assert!(COMPACT_OUTPUT_MAX_BYTES > crate::harness::title::TITLE_OUTPUT_MAX_BYTES);
            assert!(COMPACT_OUTPUT_MAX_BYTES > crate::harness::triage::TRIAGE_OUTPUT_MAX_BYTES);
            assert!(COMPACT_OUTPUT_MAX_BYTES > crate::harness::shell_duty::SHELL_OUTPUT_MAX_BYTES);
            // `DIGEST_OUTPUT_MAX_BYTES` is private and is *defined* as this.
            assert!(COMPACT_OUTPUT_MAX_BYTES > crate::harness::context::SUMMARIZER_INPUT_MAX_BYTES);
        }
        // And it is that number for a *reason* the code states, not one that
        // happens to look about right: a replacement paragraph larger than the
        // window it is making room in could never be applied, because
        // `compact_if_pressured`'s budget check would reject it. Pinning the
        // derivation is what stops a widening from silently un-testing AC-11,
        // whose enforcement assertion reads this constant and therefore moves
        // with it (the same gap `title` closes by deriving from its contract).
        assert_eq!(
            COMPACT_OUTPUT_MAX_BYTES,
            super::super::HarnessConfig::default().context_budget_bytes,
            "the ceiling is the default context byte budget, because a compaction \
             bigger than the window it makes room in is bytes spent to be thrown away"
        );
        assert_eq!(COMPACT_DUTY.ceiling_bytes(), COMPACT_OUTPUT_MAX_BYTES);
        assert_eq!(COMPACT_DUTY.category(), Category::Compact);
    }

    /// An unresolvable route is a routing failure carrying the resolver's own
    /// sentence — asserted here on the seam, because the call site's use of it is
    /// [`super::super::context`]'s.
    #[tokio::test]
    async fn an_unresolved_route_returns_the_resolvers_reason() {
        let route = DutyRoute::unresolved(
            "The 'compact' category resolves to 'local', but no local engine is loaded to \
             serve it yet.",
        );
        let err = route
            .perform("anything", &EgressProvenance::empty())
            .await
            .expect_err("an unresolved route cannot compact");
        assert!(err.contains("no local engine is loaded"), "{err}");
    }

    /// A local route really does answer, and the answer parses into a decision.
    #[tokio::test]
    async fn a_local_route_answers_with_a_compaction() {
        let route = local_route("FORGET: 1 2\nSUMMARY: the agent read src/download.rs.");
        let answer = route
            .perform(&compact_prompt(&conversation()), &EgressProvenance::empty())
            .await
            .expect("the local duty served");
        let c = read_compaction(&answer, 3).expect("the answer parses");
        assert_eq!(c.forget(), [0, 1]);
        assert!(c.summary().contains("src/download.rs"));
    }

    // -- the remote leg: the harness-owned ceiling (BR-8, AC-11) -------------

    /// A remote `compact` route over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    fn remote_route(
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
    ) -> (DutyRoute, Sent) {
        remote_duty_route(COMPACT_DUTY, boundaries, reply, repeat)
    }

    /// **AC-11, BR-8.** A provider ignoring `max_tokens` cannot grow the compact
    /// duty's buffer without limit. The assertion reads
    /// [`COMPACT_OUTPUT_MAX_BYTES`] rather than a literal, so raising the ceiling
    /// cannot silently un-test the bound.
    #[tokio::test]
    async fn a_remote_compact_duty_is_bounded_however_much_the_provider_streams() {
        // 4 KiB per delta × 64 deltas = 256 KiB offered, 32 KiB accepted.
        let chunk = format!("{} ", "e".repeat(4095));
        let (route, _sent) = remote_route(Vec::new(), &chunk, 64);
        let DutyRoute::Serves { duty, .. } = &route else {
            panic!("the fixture must resolve, or the ceiling is never exercised");
        };

        let answer = duty
            .perform("compact this", &EgressProvenance::empty())
            .await
            .expect("the remote duty served");

        assert!(
            answer.len() <= COMPACT_OUTPUT_MAX_BYTES,
            "a compact duty accepted {} bytes from a provider ignoring its token budget; the \
             ceiling is {COMPACT_OUTPUT_MAX_BYTES}",
            answer.len()
        );
        // Non-vacuity, both directions: the provider really did offer far more
        // than the bound, and the duty really did accumulate up to it — so this
        // is a cap and not a refusal.
        assert!(
            chunk.len() * 64 > COMPACT_OUTPUT_MAX_BYTES * 4,
            "the fixture must offer well over the bound"
        );
        assert!(
            answer.len() > COMPACT_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real compaction through: {} bytes",
            answer.len()
        );
    }

    // -- the boundary interaction (BR-7) ------------------------------------

    /// **The non-vacuity half** (LESSON-485). The same remote route with **no**
    /// boundary configured genuinely sends: the conversation reaches the
    /// transport and the provider's compaction comes back. Without it, the
    /// refusal below would be equally satisfied by a fixture that could never
    /// send anything at all.
    #[tokio::test]
    async fn an_unbounded_machine_sends_the_compact_prompt_and_returns_its_answer() {
        let (route, sent) = remote_route(Vec::new(), "FORGET: 1\nSUMMARY: they read a file.", 1);
        let answer = route
            .perform(&compact_prompt(&conversation()), &EgressProvenance::empty())
            .await
            .expect("with no boundary configured the duty sends");
        let c = read_compaction(&answer, 3).expect("the answer parses");
        assert_eq!(c.forget(), [0]);
        assert!(
            wire(&sent).contains("no longer fits the agent"),
            "the duty prompt never reached the transport"
        );
    }

    /// **BR-7 through the one choke point.** The conversation a compaction reads
    /// is scoped by the provenance of the *blocks themselves*, so a conversation
    /// holding a `local-only` tool result refuses the remote compaction — and the
    /// turn carries on, under the deterministic drop.
    ///
    /// Only the *provenance* differs from the test above; the boundaries and the
    /// route are otherwise identical, which is what makes this the guard holding
    /// rather than a fixture that could not send.
    #[tokio::test]
    async fn a_compaction_over_boundary_content_is_refused_before_a_byte_leaves() {
        let boundaries = vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }];
        let (route, sent) = remote_route(boundaries, "FORGET: 1\nSUMMARY: x", 1);

        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_user("rotate the key");
        ctx.push_tool_result_prov(
            "read",
            ToolProvenance::path(fixture_id("secrets/prod.env")),
            "API_KEY=sk-live-DO-NOT-LEAK",
        );
        ctx.push_model("ok");
        let provenance = crate::harness::completion::context_provenance(&ctx);

        let err = route
            .perform(&compact_prompt(ctx.blocks()), &provenance)
            .await
            .expect_err("boundary content must not be compacted remotely");

        assert!(
            wire(&sent).is_empty(),
            "boundary-derived conversation text reached the transport"
        );
        assert!(!wire(&sent).contains("sk-live-DO-NOT-LEAK"));
        assert!(err.contains("privacy boundary"), "{err}");
    }

    /// The speaker line names the tool, so the duty can tell a file read from a
    /// user turn — which is most of what "is this still needed" depends on.
    #[test]
    fn a_tool_block_is_introduced_by_its_tool() {
        let block = ContextBlock {
            role: BlockRole::Tool,
            text: "body".to_owned(),
            provenance: Provenance::Tool {
                tool: "grep".to_owned(),
                provenance: ToolProvenance::none(),
            },
        };
        assert_eq!(speaker(&block), "Tool (grep)");
    }
}
