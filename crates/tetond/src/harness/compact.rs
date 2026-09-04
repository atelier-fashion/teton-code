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
//! | **link 1** — `compact_if_pressured` returns declined before ever asking the duty | `harness::context::tests::a_routed_compaction_replaces_the_blocks_it_forgets`, `…::compaction_runs_ahead_of_the_hard_gate_not_at_it`, `runtime::duty::dispatch::compact::a_performed_compaction_announces_its_route_and_a_declined_one_does_not`, and every other compaction test in `harness::context` |
//! | **link 2** (parse) — `read_compaction` keeps the numbers it managed to read instead of failing the whole answer | `harness::compact::tests::a_partly_readable_answer_is_no_answer_at_all`, `harness::context::tests::a_half_readable_compaction_is_not_half_applied` |
//! | **link 3** (apply) — the accepted range is taken from the conversation (`blocks.len() − 1`) instead of from the offer (`CompactOffer::droppable`) | `harness::context::tests::an_answer_naming_a_block_that_was_never_offered_is_refused_not_applied` — **and nothing else**, which is how a bounded offer came to accept an answer about blocks it never rendered (verify C1) |
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
//! | the session-taint override is removed from the resolver (AC-9a) | `runtime::duty::dispatch::compact::a_tainted_session_compacts_on_the_local_tier` |
//! | `ScriptedFileEngine` loses its `compact` arm (the duty eats a scripted block) | `runtime::duty::dispatch::compact::a_compact_duty_consumes_no_scripted_block` |
//!
//! AC-9(b) — "the failure path returns its input unchanged" — is not a separate
//! row because for this duty it is not a mutation: returning the context
//! unchanged **is** the failure path (BR-4 forbids applying anything in part, and
//! forbids inventing a drop the duty did not choose). What must not be
//! unchanged is the context the *turn* ends with, and the link-4 row is the
//! mutation that proves it.

use teton_protocol::Category;

use super::budget::LOCAL_GENERATION_RESERVATION;
use super::context::{truncate_middle, ContextBlock, Provenance};
use super::duty::{DutyKind, DUTY_REQUEST_BYTES_PER_TOKEN};
use super::render::CHATML_DUTY_ENVELOPE_BYTES;
use crate::runtime::LOCAL_ENGINE_N_CTX_DEFAULT;

/// Byte ceiling on what a `compact` duty may return (BR-8).
///
/// The **loosest of the five**, deliberately: a `title` is a handful of words and
/// a `triage` is a list of numbers, but a compaction stands in for a
/// conversation. It is the **local route's own byte budget**, because a
/// replacement paragraph larger than the window it is making room in cannot
/// possibly be applied: the budget check in
/// [`ContextManager::compact_if_pressured`](super::context::ContextManager::compact_if_pressured)
/// would reject it anyway, so accumulating more than this is bytes spent to be
/// thrown away — *the duty that repairs an over-budget context may not return
/// more than the budget it is repairing to.*
///
/// # One chain, written down once (REQ-590 ADR-5, LESSON-491)
///
/// ```text
///   engine window          32,768 tokens   (LOCAL_ENGINE_N_CTX_DEFAULT)
///   − the generation        1,024 tokens   (LOCAL_GENERATION_RESERVATION)
///   = 31,744 usable        × 2 B/token     (DUTY_REQUEST_BYTES_PER_TOKEN)
///   = 63,488 bytes  — everything a prompt of this window can hold once the
///                     reply's room is set aside, which is the most a repair
///                     could ever usefully return
/// ```
///
/// LESSON-491, verbatim: *"when two budgets constrain one flow, write the chain
/// down once and derive each number from its neighbour; any two 'independent'
/// numbers on one chain are a bug waiting to happen."* This constant is link
/// three, and until REQ-590 it was pinned to link **one's** value by *name* —
/// it read `LOCAL_BUDGET_BYTES` (32,768), which is a constant chosen for a
/// route with no window at all, not a fact about this engine. It followed the
/// engine only by coincidence, and the coincidence was invisible at both
/// definition sites.
///
/// # Ceiling = budget, by construction (REQ-590 ADR-6a, restored at 32,768)
///
/// The local route's byte budget derives from the same chain —
/// [`derive`](super::budget::derive)'s local arm bridges the same usable window
/// at the same 2 B/token — so the ceiling and the budget are the same 63,488 by
/// construction, and `the_compact_ceiling_is_the_loosest_of_the_five` asserts
/// them equal as a **relation** between two derived values, never as two
/// literals that happen to agree, which is the shape that broke.
///
/// That equality was gone between REQ-590 ADR-9 and the window's raise to
/// 32,768: on the 16,384-token window the derived byte half (30,720) was
/// *smaller* than the 32,768 constant the local route had always run under,
/// so ADR-9 kept the constant and the relation was `ceiling ≤ budget` with a
/// 2,048-byte residual — the amount by which that constant out-claimed the
/// engine. At 32,768 the derived half is 63,488, nearly twice the constant, the
/// premise is gone and both numbers come off one chain again.
///
/// The one link this does *not* follow is the redact clamp or a remote route's
/// larger pair: the ceiling is the **local** budget on purpose, for the same
/// reason [`COMPACT_PROMPT_BUDGET_BYTES`] is sized to the local window —
/// `compact` sits on its default local binding for most sessions, and a
/// harness-owned bound that grew with whatever route happened to be bound would
/// be no bound at all on the route the duty actually runs on.
///
/// Enforced in the duty implementation rather than requested of the provider
/// (LESSON-484): `max_tokens` is a request, and a request is not a bound.
///
/// `saturating_sub`, not `-`, for the same reason `window_pair` — the private
/// half of [`derive`](super::budget::derive) this mirrors — uses it: a
/// reservation that swallowed
/// the whole window must land where the budget lands, not stop the build here
/// while the budget quietly derives 0. Following the neighbour's arithmetic is
/// the whole point of following the neighbour.
pub const COMPACT_OUTPUT_MAX_BYTES: usize = compact_output_max_bytes(LOCAL_ENGINE_N_CTX_DEFAULT);

/// [`COMPACT_OUTPUT_MAX_BYTES`] at an arbitrary engine window (REQ-616 BR-8).
#[must_use]
pub const fn compact_output_max_bytes(n_ctx: u32) -> usize {
    n_ctx.saturating_sub(LOCAL_GENERATION_RESERVATION) as usize * DUTY_REQUEST_BYTES_PER_TOKEN
}

/// The `compact` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`], the transport-free offline entry point in
/// [`super::turn_loop`], and the tests.
pub const COMPACT_DUTY: DutyKind = DutyKind::new(Category::Compact, COMPACT_OUTPUT_MAX_BYTES);

/// Byte ceiling on the `compact` duty's own **prompt** (REQ-586 BR-6, ADR-5).
///
/// **Derived from the engine window, not picked beside it** (LESSON-446), in the
/// same shape as
/// [`REDACT_PROMPT_BUDGET_BYTES`](crate::egress::redact::REDACT_PROMPT_BUDGET_BYTES):
///
/// ```text
///   engine window            32,768 tokens   (LOCAL_ENGINE_N_CTX_DEFAULT)
///   − the duty's generation   4,096 tokens   (COMPACT_DUTY.max_tokens())
///   = 28,672 tokens × 2 B/token             (DUTY_REQUEST_BYTES_PER_TOKEN)
///   − the ChatML envelope        55 bytes   (CHATML_DUTY_ENVELOPE_BYTES)
/// ```
///
/// Why the *local* window and not the route's: `compact` sits on its default
/// local binding for most sessions, and its prompt renders the whole
/// conversation. Before REQ-586 there was no total bound at all, which was
/// harmless while every conversation was budgeted at 32 KB — and became a
/// per-fold failure the moment a 128k route let a conversation grow past the
/// local engine's window, because `LlamaEngine::complete` refuses an
/// over-window prompt, the refusal degrades the duty, and every fold then fell
/// back to the deterministic drop the duty exists to improve on (BR-6's "keeps
/// its proportion in practice and not only in the threshold").
///
/// A remote `compact` binding has a larger window than this and is simply
/// offered less than it could take, which costs a partial offer — and a partial
/// offer still compacts, because the answer is block numbers.
pub const COMPACT_PROMPT_BUDGET_BYTES: usize =
    compact_prompt_budget_bytes(LOCAL_ENGINE_N_CTX_DEFAULT);

/// [`COMPACT_PROMPT_BUDGET_BYTES`] at an arbitrary engine window (REQ-616
/// BR-8). The `compact` duty runs on the local engine, so this follows the
/// local window exactly as the redact chain does.
#[must_use]
pub const fn compact_prompt_budget_bytes(n_ctx: u32) -> usize {
    // Saturating, because `n_ctx` is a **runtime** value since REQ-616 and both
    // subtractions underflow below it: `COMPACT_DUTY.max_tokens()` is 4,096, so
    // a window under that wrapped to an enormous budget in release and panicked
    // in debug. Reachable through `[inference] n_ctx`, which is also refused
    // below the floor now — this is the second of the two guards, and it is the
    // one that holds if a future caller finds another way in.
    (n_ctx as usize)
        .saturating_sub(COMPACT_DUTY.max_tokens() as usize)
        .saturating_mul(DUTY_REQUEST_BYTES_PER_TOKEN)
        .saturating_sub(CHATML_DUTY_ENVELOPE_BYTES)
}

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

/// The note appended to the block the duty may not forget — the **aside**, on
/// the numbered line itself.
///
/// The protected block is named twice in a prompt that offers it, and the two
/// wordings are deliberately not one string (verify D2). This one is an aside
/// hanging off a line the reader is already on ("this block"); [`offer_footer`]
/// says the same thing as a *sentence about a numbered block* ("block 200 is
/// the step in progress and cannot be forgotten"), because it has to be
/// readable when block 200 was never rendered — which is the case the footer
/// exists for. Composing both from one fragment would make one of them read as
/// a fragment.
///
/// Neither is a fact anything is derived from: the protection is enforced by
/// [`CompactOffer::droppable`] and [`read_compaction`], whatever the prompt
/// said, so a drift between these two sentences costs a badly-phrased question
/// and never a wrongly-applied answer. Both spellings are pinned verbatim —
/// the aside by `the_duty_prompt_numbers_every_block_and_protects_the_last`,
/// the sentence by `a_two_hundred_block_conversation_still_fits_the_duty_prompt`.
const PROTECTED_BLOCK_NOTE: &str = "   (the step in progress — this block cannot be forgotten)\n";

/// The line that tells the duty exactly which slice of the conversation it was
/// shown, and which block is protected (REQ-586 BR-6).
///
/// A partial offer is still a usable question — the answer is block numbers, and
/// numbers 1..`offered` mean the same blocks whether or not the rest of the
/// conversation was rendered — but only if the duty is told where the list
/// stops. Without it a model asked to compact "the conversation" while shown its
/// first twenty blocks would reasonably answer about blocks it never saw, and
/// [`read_compaction`]'s range check would reject the whole answer.
fn offer_footer(offered: usize, total: usize) -> String {
    format!(
        "(offered blocks 1..{offered} of {total}; block {total} is the step in progress and \
         cannot be forgotten)\n"
    )
}

/// A rendered `compact` offer: the prompt, and **how much of the conversation
/// it actually showed** (REQ-586 verify, C1).
///
/// The two travel together because the second is what bounds the first's
/// answer. Once the offer became a bounded prefix (BR-6), "how many blocks may
/// this answer name" stopped being a fact about the conversation and became a
/// fact about the *prompt*: a duty shown blocks 1..24 of 200 that answers
/// `FORGET: 1..150` has written about blocks nobody rendered, and applying it
/// would delete 126 blocks the duty never saw and replace them with a summary
/// written from the 24 it did. Both post-checks (over-budget, no-shrink) pass
/// for that answer — the context genuinely got smaller — so nothing downstream
/// would catch it.
///
/// [`Self::droppable`] is therefore the one number [`read_compaction`] should
/// ever be given, and it is computed here, beside the loop that decided how
/// much to render, rather than at the call site from `blocks.len()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactOffer {
    prompt: String,
    offered: usize,
    total: usize,
}

impl CompactOffer {
    /// The rendered duty prompt.
    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// How many of the conversation's blocks the prompt rendered.
    #[must_use]
    pub const fn offered(&self) -> usize {
        self.offered
    }

    /// How many blocks the duty's answer may name — the leading blocks of the
    /// offer, minus the step in progress.
    ///
    /// `min(offered, total − 1)`, and both halves are load-bearing: the offer
    /// bounds it because a block that was never rendered cannot have been
    /// chosen, and `total − 1` bounds it because the newest block is the step
    /// the turn is working on — the one block neither the duty nor
    /// `truncate_to_budget` may take. When the whole conversation fits (every
    /// pre-REQ-586 render, and every small one since) `offered == total` and
    /// this is exactly the `blocks.len() − 1` the apply step used to pass.
    #[must_use]
    pub const fn droppable(&self) -> usize {
        let protected = self.total.saturating_sub(1);
        if self.offered < protected {
            self.offered
        } else {
            protected
        }
    }
}

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
///
/// ## The offer is bounded, and the offer is stated (REQ-586 BR-6, ADR-5)
///
/// The rendered prompt fits `prompt_budget_bytes`
/// ([`COMPACT_PROMPT_BUDGET_BYTES`] at the one production call site): the
/// **oldest** blocks are offered — they are the ones a compaction is for — and
/// the list stops at the first block that would overflow. The footer then names
/// the slice (`offered blocks 1..N of M`) and the protected block, so the
/// answer's numbers still mean what they meant when the whole conversation was
/// rendered.
///
/// The protected block is named whether or not it was itself offered: a duty
/// shown blocks 1..20 of 200 must still know that 200 is off limits, and
/// `read_compaction` refuses it either way.
///
/// A conversation whose *first* block already overflows the budget still offers
/// that block — one over-budget prompt that the engine refuses is a degraded
/// fold, whereas an empty list is an unanswerable question every time.
///
/// ## The offer is also what bounds the answer (verify C1)
///
/// The returned [`CompactOffer`] carries the offered count, and
/// [`CompactOffer::droppable`] is the range [`read_compaction`] must be given.
/// Rendering a prefix while accepting an answer about the whole conversation
/// is the one way a bounded offer can *lose* a conversation rather than
/// compact it: every block between the end of the offer and the largest number
/// the answer named would be deleted unseen, and replaced by a summary written
/// from the prefix alone.
#[must_use]
pub fn compact_offer(blocks: &[ContextBlock], prompt_budget_bytes: usize) -> CompactOffer {
    let mut prompt = String::new();
    prompt.push_str(
        "Below is a numbered list of the blocks of one conversation between a person and an \
         AI coding agent. It no longer fits the agent's context window. Choose the blocks \
         whose content is no longer needed to carry on the work, and write the one paragraph \
         that preserves what a reader would still need from them. ",
    );
    prompt.push_str(COMPACT_OUTPUT_CONTRACT);
    prompt.push_str(BLOCK_LIST_HEADER);
    let total = blocks.len();
    // Reserved with `total` in both positions: `offered <= total`, so this is an
    // upper bound on the footer the loop will actually write, and the budget is
    // met whatever the loop decides.
    let footer_reserve = offer_footer(total, total).len();
    let mut offered = 0usize;
    for (i, block) in blocks.iter().enumerate() {
        let line = format!(
            "{}. {}: {}\n",
            i + 1,
            speaker(block),
            truncate_middle(&block.text, COMPACT_BLOCK_MAX_BYTES)
        );
        let protected = i + 1 == total;
        let extra = if protected {
            PROTECTED_BLOCK_NOTE.len()
        } else {
            0
        };
        if offered > 0 && prompt.len() + line.len() + extra + footer_reserve > prompt_budget_bytes {
            break;
        }
        prompt.push_str(&line);
        if protected {
            prompt.push_str(PROTECTED_BLOCK_NOTE);
        }
        offered += 1;
    }
    // An empty conversation has no slice and no protected block to name; the
    // duty declines long before this in production ([`worth_compacting`]), and a
    // footer about "block 0" would be the only nonsense in the prompt.
    if total > 0 {
        prompt.push_str(&offer_footer(offered, total));
    }
    CompactOffer {
        prompt,
        offered,
        total,
    }
}

/// The rendered prompt alone — [`compact_offer`] for a caller that will not
/// apply an answer.
///
/// The stand-in engine's recognizer and the prompt-shape tests want the string
/// and nothing else. A caller that *applies* what the duty answers must take
/// the whole [`CompactOffer`] instead: the accepted range is a fact about this
/// prompt (how much of the conversation it rendered), not about the
/// conversation, and re-deriving it from `blocks.len()` is exactly the drop C1
/// found.
#[must_use]
pub fn compact_prompt(blocks: &[ContextBlock], prompt_budget_bytes: usize) -> String {
    compact_offer(blocks, prompt_budget_bytes).prompt
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

    /// **AC-9.** The soft threshold is a *fraction*, so a 100k route crosses it
    /// at exactly the same point a 4k one does — the proportion is what REQ-586
    /// keeps, not a byte count that happened to suit the local pair.
    ///
    /// The hard gate is unchanged in kind and in number too: it fires at 100% of
    /// whichever budget the manager holds, which
    /// `harness::context::tests::a_gate_that_drops_three_blocks_reports_three_blocks`
    /// and `…::rebudget_from_a_remote_pair_to_the_local_one_drops_and_reports`
    /// exercise on both pairs. And the REQ-561 fallback — a compaction that
    /// cannot be served leaves the deterministic drop to enforce the budget — is
    /// unchanged and still pinned by
    /// `harness::context::tests::a_failed_compaction_does_not_keep_everything`
    /// (with `…::an_unrouted_compact_duty_still_ends_under_budget` for the
    /// never-routed arm) and, at the loop, by
    /// `harness::turn_loop::tests::a_turn_whose_compact_duty_cannot_serve_still_ends_under_budget`.
    #[test]
    fn a_hundred_k_budget_is_pressured_at_the_same_percent_as_a_four_k_one() {
        // The local pair's byte half, and a 100k route's — the two ends of the
        // range REQ-586 opens up.
        for budget in [
            crate::harness::budget::LOCAL_BUDGET_BYTES,
            crate::harness::budget::derive(crate::harness::budget::BudgetInputs {
                window: 100_000,
                cap: 0,
                reservation: 1_024,
                is_local: false,
                redact_scan: false,
                provider_id: Some("kimi"),
                local_window: 0,
            })
            .budget_bytes,
        ] {
            let at = budget * COMPACT_PRESSURE_PERCENT / 100;
            assert!(
                !under_pressure(at, budget),
                "budget {budget}: exactly at the threshold must not fire"
            );
            assert!(
                under_pressure(at + 1, budget),
                "budget {budget}: one byte past the threshold must fire"
            );
            assert!(
                under_pressure(budget * 4 / 5, budget),
                "budget {budget}: four-fifths must count as pressure"
            );
        }
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
        assert!(compact_prompt(&conversation(), COMPACT_PROMPT_BUDGET_BYTES)
            .contains(COMPACT_OUTPUT_CONTRACT));
    }

    /// The prompt numbers every block from 1, names its speaker, and says which
    /// one may not be forgotten — the three things the answer is written against.
    #[test]
    fn the_duty_prompt_numbers_every_block_and_protects_the_last() {
        let blocks = conversation();
        let prompt = compact_prompt(&blocks, COMPACT_PROMPT_BUDGET_BYTES);
        assert_eq!(offered_block_count(&prompt), blocks.len());
        assert!(prompt.contains("1. User: port the download client"));
        assert!(prompt.contains("3. Tool (read): fn get() {}"));
        assert!(prompt.contains("4. Assistant: I will edit it now"));
        assert!(prompt.contains("cannot be forgotten"));
        // And the offer is stated even when it is the whole conversation, so
        // the duty never has to infer where the list stops (REQ-586 BR-6).
        assert!(
            prompt.contains(
                "(offered blocks 1..4 of 4; block 4 is the step in progress and cannot be \
                 forgotten)"
            ),
            "{prompt}"
        );
    }

    /// **The prompt budget is derived from the engine window, not picked beside
    /// it** (REQ-586 ADR-5, LESSON-446) — the `REDACT_PROMPT_BUDGET_BYTES`
    /// shape.
    #[test]
    fn the_prompt_budget_is_derived_from_the_local_engine_window() {
        // The arithmetic, written out once: 32,768 − 4,096 = 28,672 tokens, at
        // 2 B/token = 57,344, less the 55-byte ChatML envelope.
        assert_eq!(
            COMPACT_PROMPT_BUDGET_BYTES, 57_289,
            "the duty's prompt budget moved; if that was deliberate, say why here"
        );
        const {
            // The generation reservation really was subtracted, and the
            // envelope really was charged.
            assert!(
                COMPACT_PROMPT_BUDGET_BYTES
                    < LOCAL_ENGINE_N_CTX_DEFAULT as usize * DUTY_REQUEST_BYTES_PER_TOKEN
            );
            // And what is left still holds a decision: more than the minimum
            // number of blocks worth asking about, at the per-block display
            // bound. A budget under this would decline every conversation by
            // rendering one block and calling it a choice.
            assert!(COMPACT_PROMPT_BUDGET_BYTES > COMPACT_MIN_BLOCKS * COMPACT_BLOCK_MAX_BYTES);
        }
    }

    /// **AC-9 / BR-6.** A 200-block conversation — what a 128k route makes
    /// reachable — renders a duty prompt the *local* engine will accept, so the
    /// fold is decided by a model rather than degrading to the deterministic
    /// drop every time.
    ///
    /// The oldest blocks are the ones offered, because they are the ones a
    /// compaction is for; the protected block is named in the footer even though
    /// it was never rendered, because `read_compaction` will refuse it either
    /// way and the duty should not spend its answer finding that out.
    #[test]
    fn a_two_hundred_block_conversation_still_fits_the_duty_prompt() {
        let mut ctx = ContextManager::new("sys", 1_000_000);
        for i in 0..200 {
            ctx.push_user(format!("block {i}: {}", "padding ".repeat(200)));
        }
        let blocks = ctx.blocks().to_vec();
        // Non-vacuity: rendered whole, this conversation is far past the window.
        assert!(blocks.len() * COMPACT_BLOCK_MAX_BYTES > COMPACT_PROMPT_BUDGET_BYTES);

        let offer = compact_offer(&blocks, COMPACT_PROMPT_BUDGET_BYTES);
        let prompt = offer.prompt();

        assert!(
            prompt.len() <= COMPACT_PROMPT_BUDGET_BYTES,
            "a 200-block conversation produced a {} byte prompt against a {} byte budget",
            prompt.len(),
            COMPACT_PROMPT_BUDGET_BYTES
        );
        let offered = offered_block_count(prompt);
        // **Verify C1.** What the answer may name is bounded by what the prompt
        // showed, not by what the conversation holds: on a partial offer the
        // two are 175 blocks apart, and every one of those blocks would be
        // deleted unseen by an answer resolved against the conversation.
        assert_eq!(
            offer.offered(),
            offered,
            "the rendered list and the reported count are one fact"
        );
        assert_eq!(offer.droppable(), offer.offered());
        assert!(
            offer.droppable() < blocks.len() - 1,
            "a partial offer must accept less than the whole conversation: \
             {} against {}",
            offer.droppable(),
            blocks.len() - 1
        );
        assert!(
            (1..200).contains(&offered),
            "a partial offer is still an offer, and an empty one is not: {offered}"
        );
        assert!(prompt.contains("1. User: block 0"), "the oldest go first");
        assert!(
            !prompt.contains("200. User: block 199"),
            "the whole conversation was rendered after all"
        );
        assert!(
            prompt.contains(&format!(
                "(offered blocks 1..{offered} of 200; block 200 is the step in progress and \
                 cannot be forgotten)"
            )),
            "the offer and the protected block must both be named: {}",
            &prompt[prompt.len().saturating_sub(200)..]
        );
        assert!(prompt.contains(COMPACT_OUTPUT_CONTRACT));
    }

    /// A conversation whose very first block already overflows the budget is
    /// still asked a question: one over-budget prompt the engine may refuse is a
    /// degraded fold, but an empty list is an unanswerable one every time.
    #[test]
    fn a_budget_too_small_for_one_block_still_offers_that_block() {
        let blocks = conversation();
        let prompt = compact_prompt(&blocks, 1);
        assert_eq!(offered_block_count(&prompt), 1);
        assert!(prompt.contains("(offered blocks 1..1 of 4;"), "{prompt}");
    }

    /// A pasted core dump cannot make the prompt unbounded: every block is
    /// display-bounded in **bytes**, the unit the wire is measured in.
    #[test]
    fn an_enormous_block_is_bounded_in_the_prompt() {
        let mut ctx = ContextManager::new("sys", 10_000);
        ctx.push_user("é".repeat(50_000));
        ctx.push_model("ok");
        ctx.push_user("and now?");
        let prompt = compact_prompt(ctx.blocks(), COMPACT_PROMPT_BUDGET_BYTES);
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
        //
        // **AC-8, and it is written as a relation on purpose.** Both sides are
        // *read*, neither is restated: the left is the ceiling, the right is
        // what `derive` hands the local route, and the assertion is the one
        // place that says they are the same chain. Two literals that happened
        // to agree is exactly what this constant was until REQ-590 — the
        // ceiling said 32,768 because the local budget had once said 32,768,
        // and when the budget moved to 30,720 nothing here noticed
        // (LESSON-491).
        let local = super::super::budget::derive(super::super::budget::BudgetInputs::local());
        assert!(
            COMPACT_OUTPUT_MAX_BYTES <= local.budget_bytes,
            "the `compact` duty may return {COMPACT_OUTPUT_MAX_BYTES} B into a context \
             budgeted at {} B: a repair may not return more than the budget it is \
             repairing to",
            local.budget_bytes
        );
        // **The equality TASK-271 first asserted stands again (ADR-6a).** A
        // ceiling *below* the budget leaves the duty unable to write the
        // paragraph its own budget could hold; a ceiling *above* it is bytes
        // spent to be thrown away. Both sides derive from one chain — the
        // engine's usable window at the 2 B/token floor — so they are the same
        // number by construction, and the relation is pinned as an equality
        // between two derived values rather than as two literals.
        //
        // Between REQ-590 ADR-9 and the window's raise to 32,768 this read
        // `ceiling ≤ budget` with a stated 2,048-byte residual, because the
        // local byte budget was then the 32,768 constant rather than the
        // window's 30,720; at 32,768 tokens the derived half is 63,488 and the
        // constant is no longer read here.
        assert_eq!(
            COMPACT_OUTPUT_MAX_BYTES, local.budget_bytes,
            "the ceiling and the local byte budget have parted company — one of the two has \
             moved without the other, which is LESSON-491's shape returning"
        );
        // The same relation through the config the turn loop actually runs on,
        // so the chain is pinned at the surface as well as at the derivation.
        assert_eq!(
            COMPACT_OUTPUT_MAX_BYTES,
            super::super::HarnessConfig::default().context_budget_bytes,
            "the ceiling ({COMPACT_OUTPUT_MAX_BYTES} B) and the local route's context byte \
             budget ({} B) are one chain, and the config no longer carries it",
            super::super::HarnessConfig::default().context_budget_bytes
        );
        assert_eq!(COMPACT_DUTY.ceiling_bytes(), COMPACT_OUTPUT_MAX_BYTES);
        assert_eq!(COMPACT_DUTY.category(), Category::Compact);
        // **The `max_tokens` half of AC-8.** The request is still sized from
        // this ceiling — `DutyKind::max_tokens` divides it by
        // `DUTY_REQUEST_BYTES_PER_TOKEN` and caps the result — so the relation
        // to assert is that it never asks for more output than the ceiling
        // would keep. At this ceiling the *cap* is what binds (pinned as
        // `COMPACT_DUTY.max_tokens() == DUTY_MAX_TOKENS_REQUEST` in `duty.rs`,
        // where that constant lives), which is why the ceiling's moves —
        // 32,768 → 30,720 in REQ-590, 30,720 → 63,488 with the window — left
        // the generation budget untouched.
        assert!(
            COMPACT_DUTY.max_tokens() as usize
                <= COMPACT_OUTPUT_MAX_BYTES / DUTY_REQUEST_BYTES_PER_TOKEN,
            "`max_tokens` ({}) asks for more than the {COMPACT_OUTPUT_MAX_BYTES}-byte \
             ceiling can keep",
            COMPACT_DUTY.max_tokens()
        );
    }

    // **`a_compaction_that_lands_in_the_old_gap_is_applied_not_degraded` was
    // removed here (REQ-590 ADR-9); the hole is deliberate and this says why.**
    //
    // TASK-271 wrote it against D-4's state: the byte budget at 30,720 with the
    // ceiling still reading `LOCAL_BUDGET_BYTES` (32,768), so an answer landing
    // in the 2,048 B between them was rejected at the budget check and the turn
    // degraded to oldest-first eviction. That band was the defect, and the test
    // drove a real duty through it.
    //
    // ADR-9 reversed D-4. The byte budget is `LOCAL_BUDGET_BYTES` again and the
    // ceiling is window-derived *below* it, so no ceiling-bounded answer can
    // exceed the budget — the ordering is structural, and
    // `the_compact_ceiling_is_the_loosest_of_the_five` pins it along with the
    // residual between the two.
    //
    // The test asked for this itself. Its own guard read: *"there is no gap to
    // test; if the default pair and the local pair have been brought back into
    // agreement, this test has nothing left to say"* — written by an author who
    // anticipated the reversal and made the test announce its obsolescence
    // rather than pass quietly on a fixture that no longer discriminates.
    // Widening the fixture until it fired again would have been manufacturing a
    // property the arithmetic has removed (LESSON-563, from the other side).

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
            .perform(
                &compact_prompt(&conversation(), COMPACT_PROMPT_BUDGET_BYTES),
                &EgressProvenance::empty(),
            )
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
            .perform(
                &compact_prompt(&conversation(), COMPACT_PROMPT_BUDGET_BYTES),
                &EgressProvenance::empty(),
            )
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
            origin: Default::default(),
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
            .perform(
                &compact_prompt(ctx.blocks(), COMPACT_PROMPT_BUDGET_BYTES),
                &provenance,
            )
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
