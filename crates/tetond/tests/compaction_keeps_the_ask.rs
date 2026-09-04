//! REQ-618 acceptance — the ask survives, across one compaction and across two
//! prompts.
//!
//! # What these three cases are, and what they are not
//!
//! AC-1, AC-7 and AC-8 are the REQ's whole-session claims: not "the anchor set
//! is computed correctly" (that is `harness::context`'s unit tests) but "after a
//! real turn assembled the way the daemon assembles one, the user's question is
//! still there, byte for byte, in what the provider is handed".
//!
//! So every assertion here reads the **assembled prompt** — `prepare()`'s
//! output, which is what a remote turn maps to `TurnRequest.system` and
//! `.messages`, and what the local source hands the engine — rather than the
//! manager's block list. A test that asserted on the blocks would pass on a
//! build whose *renderer* dropped the ask.
//!
//! # AC-8 is a reconstruction, and says so
//!
//! Session `sess-23aczryx…` is not a repository artifact. Transcripts are
//! written to a directory the daemon's own tools are forbidden to read
//! (REQ-611 ADR-7) and no `.jsonl` fixture exists in this tree. What this file
//! reconstructs is the transcript's *shape* at the figures the REQ names — the
//! 21,162-token budget, a 25 KB skill body, twenty-six tool results, the
//! `/analyze` prompt line and *"where are the results?"* — and the claim it
//! checks is the one AC-8 makes: both prompts survive verbatim into the fourth
//! prompt's request body. Nobody should read it as evidence about the original
//! file (LESSON-519, read the other way round: an assert-by-inspection AC needs
//! the real artifact, and where there is none the test must say what it has
//! instead).
//!
//! # Inversion
//!
//! Reverting the anchor guard in `truncate_to_budget` — dropping the oldest
//! block unconditionally, as before this REQ — reddens **all four** tests here.
//! Recorded rather than described: it was run, and the first run reddened only
//! three. The fourth (`the_previous_prompt_survives_into_the_next`) was passing
//! on a fixture whose turn never crossed its budget at all, so no gate ran and
//! nothing could have been lost. Its results were doubled in size and it now
//! asserts the drop it depends on. That is the whole argument for running the
//! inversion rather than reasoning about it (LESSON-598).

use std::collections::BTreeSet;

use tetond::harness::context::{Anchor, NoopProvenanceHook};
use tetond::harness::{ContextManager, ToolProvenance};

/// The budget the 2026-09-04 session ran under: REQ-616's pre-raise local pair,
/// as the REQ's Description quotes it.
const REPORTED_BUDGET_TOKENS: usize = 21_162;

/// Its byte twin, at REQ-590's 3 B/word bridge.
const REPORTED_BUDGET_BYTES: usize = REPORTED_BUDGET_TOKENS * 3;

/// The skill body's size in the reported session: 25,252 bytes of `SKILL.md`
/// plus a 3,811-byte ethos include, rounded to the figure the REQ states.
const REPORTED_BODY_BYTES: usize = 25_000;

const ANALYZE_PROMPT: &str = "/analyze the routing layer for dead code";
const FOURTH_PROMPT: &str = "where are the results?";

fn filler(bytes: usize) -> String {
    let mut s = String::with_capacity(bytes + 8);
    while s.len() < bytes {
        s.push_str("padding ");
    }
    s.truncate(bytes);
    s
}

fn manager() -> ContextManager {
    ContextManager::new("HEAD", REPORTED_BUDGET_TOKENS).with_budget_bytes(REPORTED_BUDGET_BYTES)
}

/// The assembled prompt — what the provider is handed.
fn assembled(ctx: &ContextManager) -> String {
    let mut hook = NoopProvenanceHook;
    ctx.prepare(&mut hook).flat
}

/// **AC-1.** A 25 KB skill body admitted through BR-4's offer, then 40 KB of
/// tool results. After the compaction the prompt block and the body are
/// byte-identical to what was pushed, and every block that went was a tool
/// result.
///
/// The expansion is the turn's ask on a typed `/analyze` — `CarriedTurn::begin`
/// seeds it as the prompt block, one block either way — so what this checks is
/// that the *one* block carrying both survived whole.
///
/// Read from `into_retained` as the AC asks (LESSON-519: inspect, don't infer),
/// and from the assembled prompt besides, because the retained blocks are what
/// the next turn gets and the prompt is what this one sent.
#[test]
fn the_ask_and_the_body_survive_a_compaction() {
    let mut ctx = manager();
    let body = format!("{ANALYZE_PROMPT}\n{}", filler(REPORTED_BODY_BYTES));
    ctx.push_user_from(body.clone(), BTreeSet::new(), false);

    // Twenty-six tool results, as the session ran. Each is well inside the
    // budget; together they are far past it.
    for n in 0..26 {
        ctx.push_tool_result_prov(
            "read",
            ToolProvenance::none(),
            format!("result {n}\n{}", filler(40_000 / 26)),
        );
    }
    assert!(
        ctx.estimated_bytes() > REPORTED_BUDGET_BYTES,
        "non-vacuity: the turn must be over budget, or nothing is compacted"
    );

    let report = ctx.truncate_to_budget();

    assert!(report.dropped_blocks > 0, "{report:?}");
    assert!(report.anchors_intact, "BR-1's witness: {report:?}");
    assert_eq!(
        report.elided_bytes, 0,
        "the ask is never shortened, so no bytes were elided: {report:?}"
    );

    // Every block that went was a tool result — the record says so without
    // naming any content.
    let record = report
        .as_fallback_record()
        .expect("a gate that dropped something has a record");
    assert!(
        record
            .dropped_blocks
            .iter()
            .all(|(role, _, _)| matches!(role, tetond::harness::context::BlockRole::Tool)),
        "the dropped blocks must all be tool results: {:?}",
        record.dropped_blocks
    );

    // The ask, byte-identical, read from the retained context.
    let retained = ctx.into_retained();
    let asks: Vec<&String> = retained
        .blocks()
        .iter()
        .filter(|b| b.anchor == Anchor::UserAsk)
        .map(|b| &b.text)
        .collect();
    assert_eq!(asks, vec![&body], "the ask is exactly what was pushed");
}

/// **AC-1's other half, on the wire.** The same turn, checked against the
/// assembled prompt rather than the block list — because a build that kept the
/// block and dropped it at render time would pass the assertion above.
#[test]
fn the_assembled_prompt_still_carries_the_ask() {
    let mut ctx = manager();
    let body = format!("{ANALYZE_PROMPT}\n{}", filler(REPORTED_BODY_BYTES));
    ctx.push_user_from(body, BTreeSet::new(), false);
    for n in 0..26 {
        ctx.push_tool_result_prov(
            "read",
            ToolProvenance::none(),
            format!("result {n}\n{}", filler(40_000 / 26)),
        );
    }
    let _ = ctx.truncate_to_budget();

    assert!(
        assembled(&ctx).contains(ANALYZE_PROMPT),
        "the assembled prompt lost the line the user typed"
    );
}

/// **AC-7.** Across two prompts with a compaction between them, the second
/// prompt's request body contains the first prompt's text verbatim; on the
/// third it may be summarized.
///
/// The "may be" is asserted as "is gone", which is the stronger and the
/// checkable half: BR-8 says the anchor lapses one turn later, and a rule that
/// never let go would leave nothing droppable in a long session.
#[test]
fn the_previous_prompt_survives_into_the_next() {
    const FIRST: &str = "where is the retry budget decided?";
    const SECOND: &str = "and what happens when it runs out?";
    const THIRD: &str = "show me the test that covers it";

    let mut ctx = manager();
    ctx.push_user(FIRST);
    for n in 0..26 {
        ctx.push_tool_result_prov(
            "read",
            ToolProvenance::none(),
            format!("result {n}\n{}", filler(4_000)),
        );
    }
    let report = ctx.truncate_to_budget();
    // Non-vacuity, and it was missing on the first draft of this test: at 2 KB
    // a result the whole turn fit, nothing was dropped, and "the ask survived"
    // was a statement about a gate that never ran. The inversion run is what
    // found it — three of these four tests reddened when the anchor guard was
    // reverted and this one did not.
    assert!(
        report.dropped_blocks > 0,
        "the first turn must actually compact, or the carry below is about an \
         unpressured session: {report:?}"
    );
    assert!(report.anchors_intact, "{report:?}");

    // Prompt two, on the conversation prompt one committed.
    let mut second = manager();
    second.replay(ctx.into_retained());
    second.push_user(SECOND);
    let body = assembled(&second);
    assert!(
        body.contains(FIRST),
        "the second prompt's request body must carry the first prompt verbatim — \
         this is the sentence the 2026-09-04 session lost: {body:.400}"
    );
    assert!(body.contains(SECOND));

    // Prompt three: the first ask is two prompts back and is ordinary history.
    let mut third = manager();
    third.replay(second.into_retained());
    third.push_user(THIRD);
    let anchored: Vec<&str> = third
        .blocks()
        .iter()
        .filter(|b| b.anchor == Anchor::UserAsk)
        .map(|b| b.text.as_str())
        .collect();
    assert_eq!(
        anchored,
        vec![SECOND, THIRD],
        "on the third prompt the first ask is ordinary history (BR-8)"
    );
}

/// **AC-8.** The 2026-09-04 transcript's third and fourth prompts, reconstructed
/// at the original 21,162-token budget: the fourth prompt's request body carries
/// the `/analyze` prompt line and *"where are the results?"* verbatim.
///
/// See this file's own header for what "reconstructed" means and why it is not a
/// replay. The figures are the REQ's: a 25 KB body, twenty-six tool results, and
/// the two prompt texts.
///
/// This is the case the REQ exists for. Before it, the fourth prompt was
/// answered with fourteen more directory listings because the only thing the
/// model still had was its own recent tool history.
#[test]
fn the_reconstructed_session_keeps_both_prompts() {
    // Prompt three: `/analyze`, expanded, then twenty-six tool calls.
    let mut third = manager();
    let expansion = format!("{ANALYZE_PROMPT}\n{}", filler(REPORTED_BODY_BYTES));
    third.push_user_from(expansion, BTreeSet::new(), false);
    for n in 0..26 {
        third.push_model_call(format!("{{\"tool\":\"glob\",\"call\":{n}}}"));
        third.push_tool_result_prov(
            "glob",
            ToolProvenance::none(),
            format!("listing {n}\n{}", filler(1_600)),
        );
        let _ = third.truncate_to_budget();
    }
    third.push_model("Here is what I found.");
    let _ = third.truncate_to_budget();

    // Prompt four, on the conversation prompt three committed.
    let mut fourth = manager();
    fourth.replay(third.into_retained());
    fourth.push_user(FOURTH_PROMPT);
    let report = fourth.truncate_to_budget();
    assert!(report.anchors_intact, "{report:?}");

    let body = assembled(&fourth);
    assert!(
        body.contains(ANALYZE_PROMPT),
        "the fourth prompt's request body lost the `/analyze` line — this is \
         exactly the loss the REQ was filed for: {body:.400}"
    );
    assert!(
        body.contains(FOURTH_PROMPT),
        "…and it lost the question the user had just asked: {body:.400}"
    );
}
