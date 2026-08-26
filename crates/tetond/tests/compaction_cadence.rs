//! **REQ-590 AC-11** — how many turns a local session holds before compaction fires.
//!
//! D-3 accepted that the derived budget lets "a local session hold ~2.5× more
//! conversation before anything is forgotten". This file is the measurement of
//! that claim, and it is a **CI test rather than a recorded number** because the
//! compaction trigger is pure arithmetic over the two budgets — no weights, no
//! `llama` feature, nothing this suite cannot see. (AC-10's *timings* are the
//! part that cannot run here; those live in
//! `.adlc/specs/REQ-590-engine-derived-local-context-budget/architecture.md`
//! § Measurements, taken by `teton-inference`'s `local_budget_cost` example.)
//!
//! ## The finding, stated up front
//!
//! **"~2.5× more conversation" is true of the word budget and of almost no real
//! content.** `under_compaction_pressure` is a disjunction over *both*
//! currencies, so what binds is whichever budget the content exhausts first, and
//! the crossover density is `budget_bytes / budget_tokens`:
//!
//! | | words | bytes | crosses over at |
//! |---|---|---|---|
//! | before | 4,096 | 32,768 | **8 B/word** |
//! | after | 10,240 | 32,768 | **3 B/word** |
//!
//! Before this REQ, anything under 8 bytes per whitespace word — prose (≈6),
//! source (≈4–5), essentially every local conversation — was **word**-bound.
//! After it, anything over 3 B/word is **byte**-bound, and the byte half did not
//! move. So the realised gain is not 2.5×: it decays with density, and by
//! 8 B/word it is **gone** — measured at 1.00×, exactly, because past the old
//! crossover the only guard that ever binds is one this REQ did not touch.
//!
//! Exactly, in the currency that binds: the soft byte threshold is **22,937 B
//! before and after**, while the word threshold moves 2,867 → 7,168.
//!
//! **This table was measured twice.** Under D-4 the byte half fell 32,768 →
//! 30,720, which put the byte threshold at 21,504 — a 6.25% *cut*, so a
//! byte-dense local session compacted marginally *sooner* after this REQ than
//! before it, and the 4 and 6 B/word rows read 14 and 10 rather than 15 and 11.
//! ADR-9 reversed D-4, and the difference between the two tables is the price
//! that reversal paid for itself: every row is the same or better, and none is
//! worse. Both states are recorded because a reader who finds the D-4 figures
//! in an older revision should be able to tell which measurement they are
//! holding.
//!
//! ## Why the counts are equalities
//!
//! Each row's turn count is deterministic: a fixed system head, a fixed message
//! size, a fixed density, and integer arithmetic. Nothing but a change to a
//! budget, to `COMPACT_PRESSURE_PERCENT`, or to the per-block render reserve can
//! move it — which is exactly the set of changes that should have to come and
//! read this table (LESSON-491).

use std::sync::{Arc, Mutex};

use teton_inference::{ChatFormat, Completion, Engine, EngineError, GenParams};
use teton_protocol::events::BudgetBound;
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::harness::budget::derive;
use tetond::harness::compact::COMPACT_PRESSURE_PERCENT;
use tetond::harness::{
    run_session_turn_with_source, BudgetInputs, ContextManager, DutyRoute, HarnessConfig,
    LocalEngineSource, NoopProvenanceHook, PendingPermissions, PermissionConfig, PermissionGate,
    RouteBudget, SessionEvents, ToolContext, ToolDuties, ToolRegistry, DIGEST_DUTY, SHELL_DUTY,
    TRIAGE_DUTY,
};

/// The system head these fixtures budget against — short and synthetic for the
/// same reason `context_pressure.rs` uses one: the head's *size* is an input to
/// every count below, and a head that grew with the tool registry would make
/// "seven turns" a claim about the prompt template.
const SYSTEM: &str = "You are Teton Code.";

/// Words in each user message. Roughly a pasted function or a paragraph of
/// question — small enough that the counts have resolution, large enough that a
/// session reaches pressure inside [`HORIZON`].
const WORDS_PER_MESSAGE: usize = 250;

/// Words in the scripted reply, so each turn adds a *pair* of blocks the way a
/// real session does rather than only growing on the user's side.
const WORDS_PER_REPLY: usize = 120;

/// Turns to try before giving up. Well above every count in the table; a run
/// that hits it is a failure, not a longer session.
const HORIZON: usize = 60;

/// `words` whitespace words at exactly `bytes_per_word` bytes each (the last
/// word's trailing space included in the count, so the arithmetic in the table
/// is the arithmetic here).
fn filler(words: usize, bytes_per_word: usize) -> String {
    assert!(bytes_per_word >= 2, "a word and its separator need 2 bytes");
    let unit: String = std::iter::repeat_n('a', bytes_per_word - 1)
        .chain(std::iter::once(' '))
        .collect();
    unit.repeat(words)
}

/// A local [`Engine`] that answers every turn with the same fixed-size reply.
///
/// It scripts nothing else: this file measures *when* the compaction threshold
/// is crossed, and a model that varied its reply length would be varying the
/// input to the measurement.
struct ScriptedEngine {
    reply: String,
}

impl Engine for ScriptedEngine {
    fn model_id(&self) -> &str {
        "scripted-local-3b"
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        let mut text = String::new();
        let mut completion_tokens = 0u32;
        for token in self.reply.split_inclusive(' ') {
            if completion_tokens >= params.max_tokens {
                break;
            }
            let keep_going = on_token(token);
            text.push_str(token);
            completion_tokens += 1;
            if !keep_going {
                break;
            }
        }
        let prompt_tokens = u32::try_from(prompt.split_whitespace().count()).unwrap_or(u32::MAX);
        Ok(Completion::cold(text, prompt_tokens, completion_tokens))
    }

    fn chat_format(&self) -> ChatFormat {
        ChatFormat::Flat
    }
}

/// Everything one turn needs around the production loop.
struct Fixture {
    engine: Arc<Mutex<dyn Engine>>,
    bus: Arc<EventBus>,
    tools: ToolRegistry,
    tool_ctx: ToolContext,
    cwd: std::path::PathBuf,
    session: SessionId,
}

impl Fixture {
    fn new(tag: &str, reply: String) -> Self {
        let cwd = std::env::temp_dir().join(format!(
            "teton-cadence-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&cwd).expect("scratch dir");
        Self {
            engine: Arc::new(Mutex::new(ScriptedEngine { reply })),
            bus: Arc::new(EventBus::new()),
            tools: ToolRegistry::with_builtins(),
            tool_ctx: ToolContext::new(&cwd),
            cwd,
            session: SessionId::from(format!("cadence-{tag}")),
        }
    }

    fn context(&self, budget: &RouteBudget) -> ContextManager {
        ContextManager::new(SYSTEM, budget.budget_tokens)
            .with_budget_bytes(budget.budget_bytes)
            .with_window_label(&budget.window_label)
    }

    /// One turn through the production loop — the same entry point
    /// `DaemonRuntime::run_one_attempt` drives.
    async fn turn(&self, ctx: &mut ContextManager, config: &HarnessConfig) {
        let gate = PermissionGate::new(
            self.session.clone(),
            PermissionConfig::permissive(),
            Arc::clone(&self.bus),
            Arc::new(PendingPermissions::new()),
        );
        let events = SessionEvents::new(Arc::clone(&self.bus), self.session.clone());
        let mut source = LocalEngineSource::new(
            Arc::clone(&self.engine),
            ChatFormat::Flat,
            self.session.clone(),
        );
        let digest = DutyRoute::local(DIGEST_DUTY, "local", Arc::clone(&self.engine));
        let triage = DutyRoute::local(TRIAGE_DUTY, "local", Arc::clone(&self.engine));
        let shell = DutyRoute::local(SHELL_DUTY, "local", Arc::clone(&self.engine));
        // Never reached: the walk stops at the first crossing, and the `compact`
        // duty runs only once the context is already past it.
        let compact = DutyRoute::unresolved("no `compact` binding in this fixture");
        let mut hook = NoopProvenanceHook;

        run_session_turn_with_source(
            &mut source,
            &self.tools,
            &self.tool_ctx,
            &gate,
            &events,
            ctx,
            config,
            &mut hook,
            &digest,
            &compact,
            &ToolDuties {
                triage: &triage,
                shell: &shell,
            },
        )
        .await
        .expect("the scripted turn completes");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

/// Drive a session at `bytes_per_word` under `budget` and return the turn number
/// on which the accumulated conversation first crosses the soft compaction
/// threshold.
///
/// ## The predicate is read *before* the turn, deliberately
///
/// [`ContextManager::under_compaction_pressure`] is the question the loop asks
/// of the context it is about to be handed, and the loop's own answer to a
/// *yes* is to compact — which relieves the pressure. Reading the predicate
/// after the turn would therefore be able to miss the very crossing it is
/// counting. Reading it on the context the loop is about to receive cannot: the
/// growth between readings is real (the loop appended the assistant block, and
/// truncated if it had to), and the reading itself is the loop's own gate.
async fn turns_until_pressure(tag: &str, budget: &RouteBudget, bytes_per_word: usize) -> usize {
    let fixture = Fixture::new(tag, filler(WORDS_PER_REPLY, bytes_per_word));
    let config = HarnessConfig::default().with_route_budget(budget.clone());
    let mut ctx = fixture.context(budget);
    let message = filler(WORDS_PER_MESSAGE, bytes_per_word);

    for turn in 1..=HORIZON {
        ctx.push_user(message.clone());
        if ctx.under_compaction_pressure() {
            return turn;
        }
        fixture.turn(&mut ctx, &config).await;
    }
    panic!(
        "no compaction pressure within {HORIZON} turns at {bytes_per_word} B/word \
         against ({}, {})",
        budget.budget_tokens, budget.budget_bytes
    );
}

/// The pair the local tier ran under **before** this REQ.
///
/// Taken from `derive` rather than written as two literals: `(4,096, 32,768)` is
/// still what a windowless remote route gets (REQ-586 AC-1, BR-5), so this is
/// the real pre-REQ-590 pair produced by the real classifier, and it moves if
/// that pair ever moves. Its `bound` is `DefaultUnknown` rather than
/// `LocalEngine` — irrelevant here, because compaction reads only the two
/// numbers, and asserted so a reader is not misled about what this fixture is.
fn pair_before_this_req() -> RouteBudget {
    let budget = derive(BudgetInputs {
        window: 0,
        cap: 0,
        reservation: 0,
        is_local: false,
        redact_scan: false,
        provider_id: None,
    });
    assert_eq!(budget.bound, BudgetBound::DefaultUnknown);
    assert_eq!(
        (budget.budget_tokens, budget.budget_bytes),
        (4_096, 32_768),
        "this fixture stands in for the pre-REQ-590 local pair; if the default \
         pair moved, it no longer does"
    );
    budget
}

/// The pair the local tier runs under **after** this REQ.
///
/// `(10,240, 32,768)`: the word half window-derived (D-3), the byte half the
/// constant it has always been (D-4, **reversed** — ADR-9). The two halves
/// having different sources is the thing this fixture exists to price, so both
/// numbers are pinned rather than read past.
fn pair_after_this_req() -> RouteBudget {
    let budget = derive(BudgetInputs::local());
    assert_eq!(budget.bound, BudgetBound::LocalEngine);
    assert_eq!(
        (budget.budget_tokens, budget.budget_bytes),
        (10_240, 32_768),
        "D-3's word half, and ADR-9's byte half"
    );
    budget
}

// ===========================================================================
// AC-11 — the measurement
// ===========================================================================

/// **AC-11.** Turns-until-`under_compaction_pressure` on a real multi-turn local
/// session, before and after, across four content densities.
///
/// The densities are chosen to straddle both crossover points (3 B/word after,
/// 8 B/word before) rather than to flatter the change:
///
/// | B/word | stands for |
/// |---|---|
/// | 4 | source — `budget.rs` measures Rust at the dense end of the ratio |
/// | 6 | prose — the density `context.rs` cites for ordinary English |
/// | 8 | punctuation- and indent-heavy text; also the *old* crossover exactly |
/// | 20 | minified JSON / base64 / path-heavy logs — `budget.rs:55-62`'s classes |
///
/// Every count is printed as well as asserted, because this test **is** the
/// AC-11 record: `cargo test -p tetond --test compaction_cadence -- --nocapture`
/// reproduces the table in the architecture doc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_before_compaction_fires_before_and_after() {
    let before_budget = pair_before_this_req();
    let after_budget = pair_after_this_req();

    // (bytes per word, expected turns before, expected turns after)
    let rows: [(usize, usize, usize); 4] = [(4, 9, 15), (6, 9, 11), (8, 8, 8), (20, 4, 4)];

    println!(
        "\nAC-11 — turns until compaction fires ({} words + {} reply words per turn)",
        WORDS_PER_MESSAGE, WORDS_PER_REPLY
    );
    println!(
        "| B/word | before ({} w / {} B) | after ({} w / {} B) | ratio |",
        before_budget.budget_tokens,
        before_budget.budget_bytes,
        after_budget.budget_tokens,
        after_budget.budget_bytes
    );
    println!("|---|---|---|---|");

    let mut measured = Vec::new();
    for (bytes_per_word, _, _) in rows {
        let before = turns_until_pressure(
            &format!("b{bytes_per_word}"),
            &before_budget,
            bytes_per_word,
        )
        .await;
        let after =
            turns_until_pressure(&format!("a{bytes_per_word}"), &after_budget, bytes_per_word)
                .await;
        println!(
            "| {bytes_per_word} | {before} | {after} | {:.2}× |",
            after as f64 / before as f64
        );
        measured.push((bytes_per_word, before, after));
    }

    // Asserted after the whole table is printed, so a run that moves a row shows
    // every row rather than stopping at the first.
    let expected: Vec<(usize, usize, usize)> = rows.into_iter().collect();
    assert_eq!(
        measured, expected,
        "the AC-11 record moved. That is legitimate only if a budget, \
         COMPACT_PRESSURE_PERCENT, or the per-block render reserve changed — \
         update the table in architecture.md § Measurements along with these \
         rows, rather than the rows alone"
    );

    // The claim D-3 made, tested where it is true and where it is not.
    let (_, sparse_before, sparse_after) = measured[0];
    assert!(
        sparse_after > sparse_before,
        "at 4 B/word the derived budget must hold strictly more conversation: \
         {sparse_before} → {sparse_after}"
    );

    // **The ceiling on the gain, which ADR-9 changed the reason for but not the
    // shape of.** At and above the old crossover density the byte half binds
    // both before and after, and that half did not move — so these rows must
    // come out *identical*, not merely no better. Under D-4 the byte half fell
    // 6.25% and the same rows were pinned `after <= before`; the equality is the
    // stronger statement the reversal earns, and a row that improved here would
    // mean the byte budget rose, which is a decision and not a refactor.
    assert_eq!(
        after_budget.budget_bytes, before_budget.budget_bytes,
        "ADR-9: the byte half is the same number before and after — the equality below is only \
         warranted while that holds"
    );
    for (bytes_per_word, before, after) in measured.iter().copied().filter(|(d, _, _)| *d >= 8) {
        assert_eq!(
            after, before,
            "at {bytes_per_word} B/word the byte half binds both before and after and it did not \
             move, so the count cannot change either: {before} → {after}"
        );
    }

    // And the gain is gone by the old crossover density, not merely smaller.
    let (_, dense_before, dense_after) = measured[2];
    assert_eq!(
        (dense_before, dense_after),
        (8, 8),
        "at 8 B/word — the density at which the old pair's two guards agree — the \
         derived budget buys no extra conversation at all"
    );
}

/// **AC-11, the mechanism behind the table.** The crossover density — which of
/// the two guards binds — is `budget_bytes / budget_tokens`, and this REQ moves
/// it from 8 B/word to 3 B/word.
///
/// Asserted separately from the turn counts because it is the *reason* the counts
/// do not scale with the word budget, and because it is a one-line relation
/// between the two halves of a pair that were chosen for different reasons —
/// LESSON-491's "write the chain down once" applied to the two guards rather
/// than to the two budgets.
#[test]
fn the_binding_guard_crosses_over_at_a_much_lower_density_after_this_req() {
    let before = pair_before_this_req();
    let after = pair_after_this_req();

    assert_eq!(
        before.budget_bytes / before.budget_tokens,
        8,
        "before this REQ, content under 8 B/word was word-bound"
    );
    assert_eq!(
        after.budget_bytes / after.budget_tokens,
        3,
        "after it, content over 3 B/word is byte-bound — which is all real content"
    );
    // The crossover moved because the **word** half rose, not because the byte
    // half fell. Under D-4 both moved and this line read `after.budget_bytes <
    // before.budget_bytes`; ADR-9 reversed the byte half, so what is true now is
    // that the half which binds after this REQ is the half that never moved —
    // which is why the turn counts at 8 and 20 B/word come out identical rather
    // than worse.
    assert_eq!(
        after.budget_bytes, before.budget_bytes,
        "ADR-9: the byte half — the half that binds on all real content after this REQ — is \
         unchanged, so the crossover moved entirely on the word half"
    );
    assert!(
        after.budget_tokens > before.budget_tokens,
        "and the word half is the half that moved: {} → {}",
        before.budget_tokens,
        after.budget_tokens
    );

    // The soft thresholds themselves, which the turn counts only approximate:
    // integer turns cannot resolve a move this small, and here there is nothing
    // to resolve on the byte half — it does not move at all (ADR-9 reversed
    // D-4's 6.25% cut), while the word half's is 2.5× up.
    let pressure = |budget: usize| budget * COMPACT_PRESSURE_PERCENT / 100;
    assert_eq!(
        (
            pressure(before.budget_tokens),
            pressure(before.budget_bytes)
        ),
        (2_867, 22_937),
        "the soft threshold before this REQ"
    );
    assert_eq!(
        (pressure(after.budget_tokens), pressure(after.budget_bytes)),
        (7_168, 22_937),
        "and after it — the word half up 2.5×, the byte half not at all. D-4 would have put the \
         byte threshold at 21,504, compacting a byte-dense local session *sooner* than before \
         this REQ; ADR-9's reversal is what leaves it where it was"
    );
}
