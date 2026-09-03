//! **REQ-586 acceptance: nothing is clamped in silence** (TASK-193) — the
//! daemon half of AC-10, plus AC-9's 200-block compaction on a big route.
//!
//! ## What lives here and why it is not a unit test
//!
//! `truncate_to_budget` returning a [`PressureReport`] is pinned in
//! `harness::context`; that the report *becomes an event* is a claim about the
//! turn loop, and about the two gates a report can reach it through. So every
//! test in this file drives the **real** loop
//! ([`run_session_turn_with_source`] — the same entry point
//! `DaemonRuntime::run_one_attempt` drives) over a scripted engine, with a real
//! [`EventBus`], and reads what a client would have received. Nothing here
//! inspects a return value the loop kept to itself.
//!
//! | Claim | Test |
//! |---|---|
//! | AC-10, three drops → **one** `context_pressure { blocks_dropped: 3 }` | [`three_dropped_blocks_are_one_event_naming_all_three`] |
//! | AC-10, an elided newest user block → the event **and** a turn notice | [`an_elided_newest_user_message_is_an_event_and_a_notice_in_the_turns_output`] |
//! | BR-7, the in-prompt marker names the **route's** window | (same test — the marker the engine was handed) |
//! | AC-9, a 200-block conversation on a 128k route compacts through the local binding | [`a_two_hundred_block_conversation_on_a_big_route_compacts_through_the_local_binding`] |
//!
//! ## Removing either emission fails
//!
//! AC-10 asks for that in as many words, and it is a property of how the second
//! test is written rather than a remark: it asserts the event *and* the notice
//! from one turn, so deleting `SessionEvents::context_pressure` from
//! `announce_pressure` reddens it, and deleting the `agent_message` sentence
//! beneath it reddens it too. The first test asserts the count as an equality
//! (`== 1`), so a gate that announced per iteration fails it just as an emitter
//! that announced never does.
//!
//! ## Bytes per word, stated
//!
//! Every filler block here is built by [`filler`] at **4 bytes per whitespace
//! word** (three characters and a space), and each fixture says which of the two
//! guards it means to press. That matters because the budget is a pair: a
//! fixture that quotes a word count while the byte guard is what actually fired
//! is testing something other than what it claims (REQ-586 Phase-3 F-19).

use std::sync::{Arc, Mutex};

use teton_inference::{ChatFormat, Completion, Engine, EngineError, GenParams};
use teton_protocol::events::{
    BudgetBound, ContextPressure, ContextPressureKind, Event, SessionUpdatePayload,
};
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::carry::CarriedTurn;
use tetond::harness::budget::derive;
use tetond::harness::compact::{
    compact_prompt, offered_block_count, COMPACT_OUTPUT_CONTRACT, COMPACT_PROMPT_BUDGET_BYTES,
};
use tetond::harness::context::PressureReport;
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, BudgetInputs, ContextManager, DutyRoute,
    HarnessConfig, LocalEngineSource, NoopProvenanceHook, PendingPermissions, PermissionConfig,
    PermissionGate, RouteBudget, SessionEvents, ToolContext, ToolDuties, ToolRegistry,
    COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TRIAGE_DUTY,
};
use tetond::runtime::SessionTaint;
use tetond::sessions::SessionRegistry;

/// The system head these fixtures budget against.
///
/// Short and synthetic rather than [`build_system_prompt`](tetond::harness::build_system_prompt)'s
/// real one: the head's *size* is an input to every arithmetic below, and a head
/// that changes with the tool registry would make "exactly three blocks were
/// dropped" a claim about the prompt template. What the head contains is pinned
/// by `template_smoke.rs`; what it costs is pinned by the margin tests.
const SYSTEM: &str = "You are Teton Code.";

/// A filler block of `words` whitespace words at **exactly 4 bytes per word**
/// (`"abc "`), so a fixture can say which of the two budget guards it is
/// pressing and be right.
///
/// 4 B/word is denser than prose (≈6) and far sparser than a minified tool
/// result (20–45 B/"word"), which puts it where the **byte** guard binds on any
/// window-derived pair: bytes ÷ 2 > words × 3/2 for anything above 3 B/word.
///
/// That now includes the local pair. It was 8 B/word (4,096 / 32,768), where
/// both guards bound at once; since REQ-590 the local tier's *word* half
/// derives from the engine's window like every other route (21,162 / 63,488),
/// so its crossover is 3 B/word too and a 4 B/word fixture presses the byte
/// guard here as well. The byte half itself did not move — D-4 briefly took it
/// to 30,720 and ADR-9 reversed that — so the crossover moved entirely on the
/// word half.
fn filler(words: usize) -> String {
    let mut s = String::with_capacity(words * 4);
    for _ in 0..words {
        s.push_str("abc ");
    }
    s.pop();
    s
}

/// A scripted local [`Engine`] that records every prompt it was handed.
///
/// It answers a `compact` duty off-script — **a duty is not a turn** (REQ-561
/// BR-10) — and refuses any duty prompt larger than
/// [`COMPACT_PROMPT_BUDGET_BYTES`], which is how the local engine's own
/// over-window refusal is modelled here without a second copy of its `n_ctx`
/// (that constant is crate-private; the byte bound derived from it is the
/// public one, and it is the bound `compact_prompt` is actually given).
struct ScriptedEngine {
    reply: String,
    /// Every prompt served, turn and duty alike, in order.
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEngine {
    fn new(reply: &str) -> (Self, Arc<Mutex<Vec<String>>>) {
        let prompts = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                reply: reply.to_owned(),
                prompts: Arc::clone(&prompts),
            },
            prompts,
        )
    }
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
        self.prompts
            .lock()
            .expect("prompt log mutex")
            .push(prompt.to_owned());

        let answer = if prompt.contains(COMPACT_OUTPUT_CONTRACT) {
            // The local engine refuses an over-window prompt rather than
            // truncating it, and that refusal is what degrades the duty. Modelled
            // here so an unbounded `compact_prompt` is a red test rather than a
            // slow one (REQ-586 BR-6, ADR-5).
            if prompt.len() > COMPACT_PROMPT_BUDGET_BYTES {
                return Err(EngineError::Backend(format!(
                    "prompt of {} bytes exceeds this engine's window ({COMPACT_PROMPT_BUDGET_BYTES})",
                    prompt.len()
                )));
            }
            // Answer about the slice actually offered, never about blocks this
            // duty was not shown (BR-6's partial offer).
            let offered = offered_block_count(prompt);
            assert!(
                offered >= 2,
                "a bounded offer must still be a question worth asking: {offered} blocks"
            );
            let forget: Vec<String> = (1..offered).map(|n| n.to_string()).collect();
            format!(
                "FORGET: {}\nSUMMARY: the earlier turns walked the retry helper and its manifest.",
                forget.join(", ")
            )
        } else {
            self.reply.clone()
        };

        let mut text = String::new();
        let mut completion_tokens = 0u32;
        for token in answer.split_inclusive(' ') {
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
}

/// Everything one turn needs around the loop, so a test reads as the claim it
/// makes rather than as ten lines of wiring.
struct Fixture {
    engine: Arc<Mutex<dyn Engine>>,
    prompts: Arc<Mutex<Vec<String>>>,
    bus: Arc<EventBus>,
    tools: ToolRegistry,
    tool_ctx: ToolContext,
    cwd: std::path::PathBuf,
    session: SessionId,
}

impl Fixture {
    fn new(tag: &str, reply: &str) -> Self {
        let (engine, prompts) = ScriptedEngine::new(reply);
        let cwd = std::env::temp_dir().join(format!(
            "teton-pressure-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&cwd).expect("scratch dir");
        Self {
            engine: Arc::new(Mutex::new(engine)),
            prompts,
            bus: Arc::new(EventBus::new()),
            tools: ToolRegistry::with_builtins(),
            tool_ctx: ToolContext::new(&cwd),
            cwd,
            session: SessionId::from(format!("pressure-{tag}")),
        }
    }

    /// A manager seeded with [`SYSTEM`] and `budget`'s pair, exactly as
    /// `CarriedTurn::begin` seeds one: both currencies and the window label from
    /// the one [`RouteBudget`] the router derived.
    fn context(&self, budget: &RouteBudget) -> ContextManager {
        ContextManager::new(SYSTEM, budget.budget_tokens)
            .with_budget_bytes(budget.budget_bytes)
            .with_window_label(&budget.window_label)
    }

    /// Run one turn through the production loop.
    ///
    /// `compact` is deliberately [`DutyRoute::unresolved`]: these fixtures are
    /// about the unconditional gate underneath the duty (REQ-561 ADR-4), and a
    /// duty that rewrote the block list would decide how many blocks the gate
    /// then had to drop. It degrades once and latches, printing the REQ-561
    /// line — which is production behaviour for a `compact` binding that cannot
    /// be routed, not fixture noise.
    async fn turn(&self, ctx: &mut ContextManager, config: &HarnessConfig) -> String {
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
        let compact = DutyRoute::unresolved("no `compact` binding in this fixture");
        let mut hook = NoopProvenanceHook;

        let outcome = run_session_turn_with_source(
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
        outcome.final_text
    }

    /// The `compact` duty route these fixtures use when the duty is the subject
    /// rather than the scenery.
    fn compact_route(&self) -> DutyRoute {
        DutyRoute::local(COMPACT_DUTY, "local", Arc::clone(&self.engine))
    }

    /// The prompt the engine was handed for its `n`th call.
    fn prompt(&self, n: usize) -> String {
        self.prompts.lock().expect("prompt log mutex")[n].clone()
    }

    fn prompts_matching(&self, needle: &str) -> Vec<String> {
        self.prompts
            .lock()
            .expect("prompt log mutex")
            .iter()
            .filter(|p| p.contains(needle))
            .cloned()
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

/// Everything one turn published, split into the two surfaces AC-10 names.
struct Published {
    pressure: Vec<ContextPressure>,
    /// The `agent_message_chunk` texts, in order.
    chunks: Vec<String>,
}

/// Drain `sub` without a wall-clock timeout: `EventBus::publish` is synchronous
/// and the turn has already returned, so everything it published is queued
/// (LESSON-450).
fn drain(sub: &mut tetond::broadcast::Subscription) -> Published {
    let mut pressure = Vec::new();
    let mut chunks = Vec::new();
    while let Some(envelope) = sub.try_recv() {
        match envelope.event {
            Event::ContextPressure(p) => pressure.push(p),
            Event::SessionUpdate(update) => {
                if let SessionUpdatePayload::AgentMessageChunk { text } = update.update {
                    chunks.push(text);
                }
            }
            _ => {}
        }
    }
    assert!(
        !sub.is_lagged(),
        "the subscription was evicted for falling behind, so an absent event \
         here would prove nothing"
    );
    Published { pressure, chunks }
}

// ===========================================================================
// AC-10 — three drops are one event, and it names all three
// ===========================================================================

/// **AC-10 / BR-7.** A turn whose carried conversation forces
/// `truncate_to_budget` to drop three blocks publishes **exactly one**
/// `context_pressure`, of kind `blocks_dropped`, carrying `dropped_blocks: 3`
/// and the route's own pair and bound.
///
/// ## The shape of the fixture
///
/// Three blocks, each a quarter past the local word half at 4 B/word (26,452
/// words / ~106 KB on the 32,768-token window), under the **local** pair — the
/// real one, `derive(BudgetInputs::local())`, not a number invented here — plus
/// a short newest user message. Both guards are over on every block alone, so
/// the gate cannot stop early. Three drops leave one block, which
/// is where the loop stops by construction (`truncate_to_budget` never drops the
/// most recent), and that one block is small enough that nothing is elided in
/// place — so this fixture's report is a clean `dropped_blocks: 3` rather than a
/// drop-and-clamp whose kind would be the same but whose story is not.
///
/// ## Why the count is an equality
///
/// The gate runs at the top of every loop iteration *and* on every exit, so the
/// interesting failure is not "no event" — it is "an event per pass", which a
/// client cannot tell from three separate clamps. `== 1` is the assertion that
/// distinguishes them; `is_quiet()` in `announce_pressure` is what makes it
/// true.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_dropped_blocks_are_one_event_naming_all_three() {
    let fixture = Fixture::new("drops", "Answered from what is left.");
    let budget = derive(BudgetInputs::local());
    assert_eq!(budget.bound, BudgetBound::LocalEngine);
    let config = HarnessConfig::default().with_route_budget(budget.clone());

    let mut ctx = fixture.context(&budget);
    // Three blocks, not three pairs: the drop loop counts blocks, and a fixture
    // that pushed a model reply between each paste would be asserting `3` about
    // a list of six. Each block is sized off the derived pair — a quarter past
    // the word half, at 4 B/word — so any one of them alone busts both halves
    // and the gate cannot stop early. (It read `12_500` while the pair was
    // 10,240 w / 32,768 B; on the 32,768-token window a 50 KB block fits, and
    // the third drop this test is named for stopped happening.)
    let block_words = budget.budget_tokens + budget.budget_tokens / 4;
    assert!(
        block_words * 4 > budget.budget_bytes,
        "fixture: one block must bust the byte half on its own"
    );
    for n in 0..3 {
        ctx.push_user(format!("paste {n}: {}", filler(block_words)));
    }
    ctx.push_user("what did those three files have in common?");
    assert_eq!(ctx.blocks().len(), 4);

    let mut sub = fixture.bus.subscribe(1_024);
    let answer = fixture.turn(&mut ctx, &config).await;
    assert_eq!(answer, "Answered from what is left.");
    let published = drain(&mut sub);

    assert_eq!(
        published.pressure.len(),
        1,
        "the gate runs on every iteration and on every exit; a quiet report is \
         not news, so a turn that clamped once announces once: {:?}",
        published.pressure
    );
    let event = &published.pressure[0];
    assert_eq!(event.kind, ContextPressureKind::BlocksDropped);
    assert_eq!(
        event.dropped_blocks, 3,
        "three blocks went and the event must say three, not 'some'"
    );
    assert_eq!(
        event.elided_bytes, 0,
        "nothing was clamped in place, so nothing may claim it was"
    );
    assert!(!event.newest_user_elided);
    // The numbers on the event are the route's own, not a second reading of the
    // manager's fields (BR-8, AC-12).
    assert_eq!(event.budget_tokens, budget.budget_tokens as u64);
    assert_eq!(event.budget_bytes, budget.budget_bytes as u64);
    assert_eq!(event.bound, budget.bound);

    // Dropping older turns is an event, not a turn notice: the extra sentence is
    // reserved for the one case where the model answers a prompt the user did
    // not send.
    assert!(
        !published.chunks.iter().any(|c| c.contains("[note:")),
        "a dropped-history turn must not tell the user their own message was \
         cut: {:?}",
        published.chunks
    );

    // Non-vacuity: what the engine was actually handed fits the budget, and
    // carries the honesty note that says history is missing.
    let prompt = fixture.prompt(0);
    assert!(
        prompt.len() <= budget.budget_bytes,
        "the gate exists to bound this: {} bytes against {}",
        prompt.len(),
        budget.budget_bytes
    );
    assert!(
        prompt.contains("[earlier conversation truncated"),
        "the model must be told history is missing: {}",
        &prompt[..prompt.len().min(400)]
    );
}

// ===========================================================================
// AC-10 — an elided newest user message is news twice over
// ===========================================================================

/// **AC-10 / BR-7.** A single user message too large for the route's byte budget
/// is middle-elided in place, and that is reported **twice**: as
/// `context_pressure { kind: block_elided, newest_user_elided: true }` and as a
/// one-line notice in the turn's own output — because an event a client may
/// render in a status line is not where "the model is answering a shortened
/// version of what you sent" belongs.
///
/// ## The route is a real 128k one
///
/// The budget comes from `derive` for a provider declaring `max_context =
/// 128_000`, so the pair here (84,650 words / 253,952 bytes) is the pair
/// production computes, and the fixture pastes 75,000 words at 4 B/word —
/// **under** the word budget and **over** the byte budget. That is deliberate:
/// on a window-derived pair the 2 B/token byte floor is what binds for prose and
/// code, and a fixture that busted the word guard instead would be testing the
/// local shape while claiming a remote one.
///
/// ## And the marker names Kimi's window, not the local engine's
///
/// The third assertion is BR-7's other half. Before REQ-586 the in-prompt
/// elision marker hard-coded "the local context window", which on this route is
/// simply false — and it is the sentence REQ-585's oversized-skill refusal is
/// built on. The marker the engine was handed is read back here, out of the
/// assembled prompt, rather than out of the manager's field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_elided_newest_user_message_is_an_event_and_a_notice_in_the_turns_output() {
    let fixture = Fixture::new("elided", "I answered the shortened version.");
    let budget = derive(BudgetInputs {
        window: 128_000,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });
    assert_eq!(budget.bound, BudgetBound::Window);
    assert_eq!(budget.window_label, "kimi's context window");
    let config = HarnessConfig::default().with_route_budget(budget.clone());

    // 75,000 words × 4 B/word = 300,000 bytes: under the 84,650-word budget and
    // over the 253,952-byte one, so the byte guard is what clamps.
    let pasted = filler(75_000);
    assert!(
        pasted.split_whitespace().count() < budget.budget_tokens
            && pasted.len() > budget.budget_bytes,
        "this fixture is about the byte guard: {} words / {} bytes against \
         {} / {}",
        pasted.split_whitespace().count(),
        pasted.len(),
        budget.budget_tokens,
        budget.budget_bytes
    );
    let mut ctx = fixture.context(&budget);
    ctx.push_user(&pasted);
    assert_eq!(ctx.blocks().len(), 1, "nothing here is droppable");

    let mut sub = fixture.bus.subscribe(1_024);
    fixture.turn(&mut ctx, &config).await;
    let published = drain(&mut sub);

    // --- Surface one: the event.
    //
    // The elision is the *first* thing announced, and it is the only thing
    // announced about the user's own message. A second, later event is expected
    // and is not this one: the clamp fills the byte budget exactly, so once the
    // model's reply is appended the exit gate has to drop the oldest block to
    // make room for it — a genuinely separate clamp, at a different point in the
    // turn, correctly announced separately rather than folded into this one.
    let event = published
        .pressure
        .first()
        .expect("the clamp must be announced");
    assert_eq!(event.kind, ContextPressureKind::BlockElided);
    assert_eq!(
        published
            .pressure
            .iter()
            .filter(|p| p.newest_user_elided)
            .count(),
        1,
        "the user's message was clamped once and must be named once: {:?}",
        published.pressure
    );
    assert_eq!(
        event.dropped_blocks, 0,
        "there was only ever one block, so nothing was dropped"
    );
    assert!(
        event.elided_bytes > 0,
        "an elision that took no bytes is not an elision"
    );
    assert!(
        event.newest_user_elided,
        "the clamped block was the user's own newest message, and that is the \
         whole reason this case is louder than the others"
    );
    assert_eq!(event.budget_bytes, budget.budget_bytes as u64);
    assert_eq!(event.bound, BudgetBound::Window);

    // --- Surface two: the turn's own output.
    let notices: Vec<&String> = published
        .chunks
        .iter()
        .filter(|c| c.contains("[note:"))
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "exactly one notice, in the transcript the user is reading: {:?}",
        published.chunks
    );
    let notice = notices[0];
    assert!(
        notice.contains("your message did not fit kimi's context window"),
        "the notice must name the window that ran out: {notice}"
    );
    assert!(
        notice.contains(&format!("({} bytes)", event.elided_bytes)),
        "the notice and the event must agree on how much went: {notice}"
    );
    assert!(
        !notice.contains("abc abc"),
        "the notice says how much was cut and never what was cut (BR-11): {notice}"
    );

    // --- And the sentence the *model* reads names the same window.
    let prompt = fixture.prompt(0);
    assert!(
        prompt.contains("kimi's context window"),
        "the in-prompt elision marker must name the route's window; the \
         pre-REQ-586 marker said 'the local context window' on every route"
    );
    assert!(
        !prompt.contains("the local context window"),
        "…and must not still say the local engine's on a 128k remote route"
    );
    assert!(
        prompt.len() <= budget.budget_bytes,
        "the clamp exists to bound this: {} bytes against {}",
        prompt.len(),
        budget.budget_bytes
    );
}

// ===========================================================================
// AC-9 — a 200-block conversation on a 128k route still compacts locally
// ===========================================================================

/// **AC-9 / BR-6, the engine-level case** (inherited from TASK-187, whose
/// unit-level equivalent pins the prompt bound alone).
///
/// A 200-block conversation assembled on a 128k route is compacted **through the
/// local `compact` binding** — the duty is asked, its prompt fits the local
/// engine's window, its answer is applied — rather than degrading to the
/// deterministic drop on every fold.
///
/// ## What would happen without the bound
///
/// `compact_prompt` renders every block (up to 1 KiB each) and before REQ-586
/// had no total bound at all. That was harmless while every conversation was
/// budgeted at 32 KB and became a per-fold failure the moment a 128k route let
/// one grow past the *local* engine's window: `LlamaEngine::complete` refuses an
/// over-window prompt, the refusal degrades the duty, and every fold then fell
/// back to the deterministic drop the duty exists to improve on. The scripted
/// engine here refuses exactly that way, so an unbounded offer is a red test.
///
/// ## Bytes per word
///
/// 200 blocks of 250 words at 4 B/word — 1,000 bytes each, 200 KB in total. The
/// **byte** guard is the one over its soft threshold (200 KB against 70% of
/// 253,952 = 177,766); the word total (50,000) is comfortably under 70% of
/// 84,650, which is the honest shape of a big-route conversation and the reason
/// the compaction is bought by bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_hundred_block_conversation_on_a_big_route_compacts_through_the_local_binding() {
    let fixture = Fixture::new("compact200", "unused");
    let budget = derive(BudgetInputs {
        window: 128_000,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });

    let mut ctx = fixture.context(&budget);
    for n in 0..100 {
        ctx.push_user(format!("step {n}: {}", filler(250)));
        ctx.push_model(format!("done {n}: {}", filler(250)));
    }
    assert_eq!(ctx.blocks().len(), 200);
    assert!(
        ctx.under_compaction_pressure(),
        "the fixture must actually be pressured, or the duty declines and this \
         test asserts nothing: {} bytes against a {}-byte budget",
        ctx.estimated_bytes(),
        budget.budget_bytes
    );
    // Non-vacuity for the bound itself: rendering this conversation whole is
    // several times what the local engine could be handed (~205 KB against a
    // 57,289-byte prompt budget; it was 4× the 24,521 of the 16,384-token
    // window).
    let unbounded = compact_prompt(ctx.blocks(), usize::MAX).len();
    assert!(
        unbounded > COMPACT_PROMPT_BUDGET_BYTES * 3,
        "an offer that would have fitted anyway proves nothing about the bound: \
         {unbounded} against {COMPACT_PROMPT_BUDGET_BYTES}"
    );

    let outcome = ctx.compact_if_pressured(&fixture.compact_route()).await;

    assert!(
        !outcome.degraded,
        "the duty was asked and its answer had to be usable — this is the \
         degrade-every-fold regression BR-6 closes: {:?}",
        outcome.reason
    );
    assert!(
        outcome.dropped_blocks > 0,
        "a compaction that forgot nothing is the deterministic drop wearing the \
         duty's clothes"
    );

    // The duty was genuinely asked, once, with a prompt the local engine can
    // take — and one that starts at the *oldest* block, which is the end a
    // partial offer must cover for the answer to be worth anything.
    let offers = fixture.prompts_matching(COMPACT_OUTPUT_CONTRACT);
    assert_eq!(offers.len(), 1, "one model call, not one per block");
    let offer = &offers[0];
    assert!(
        offer.len() <= COMPACT_PROMPT_BUDGET_BYTES,
        "the duty's prompt must fit the window it is bound to: {} against {}",
        offer.len(),
        COMPACT_PROMPT_BUDGET_BYTES
    );
    let offered = offered_block_count(offer);
    assert!(
        (2..200).contains(&offered),
        "a bounded offer is a prefix, not the whole list and not nothing: \
         {offered} of 200"
    );
    assert!(
        offer.contains("1. User: step 0:"),
        "the offer must start at the oldest block — the end compaction is for"
    );
    assert!(
        offer.contains(&format!("(offered blocks 1..{offered} of 200; block 200")),
        "a partial offer has to say where it stops and which block is protected"
    );

    // And block 200 survived: the step in progress is the one block neither the
    // duty nor the gate may take.
    let blocks = ctx.blocks();
    assert!(
        blocks
            .last()
            .expect("blocks remain")
            .text
            .contains("done 99"),
        "the protected block must still be the newest one"
    );
    assert_eq!(
        blocks.len(),
        200 - outcome.dropped_blocks + 1,
        "the forgotten blocks were replaced by exactly one summary"
    );
}

// ===========================================================================
// BR-7 — the seeding path stamps the label, not just the fixtures
// ===========================================================================

/// **Verify M7-a.** The window label reaches the marker through the path
/// production actually takes: [`CarriedTurn::begin`].
///
/// Every other marker test in this suite (and the fixture above) seeds a
/// [`ContextManager`] by hand with `.with_window_label(..)`, which is a
/// re-implementation of one line of `CarriedTurn::begin` — so deleting that
/// line from `carry.rs` left the whole package green while every real turn's
/// marker went back to naming the local engine's window on a remote route.
/// This test never calls `with_window_label`: it hands `begin` a
/// [`HarnessConfig`] carrying the route's [`RouteBudget`] and reads the marker
/// out of the prompt the engine was served, which is the only way the stamp can
/// have got there.
///
/// ## The shape
///
/// A deliberately narrow remote window (30,000 provider tokens → 19,317 words /
/// 57,952 bytes), so one pasted block busts the byte half and is clamped in
/// place with the marker. The whole conversation is the new prompt — nothing
/// carried — because what is under test is the seeding, not the replay.
///
/// **The window moved with REQ-612's floor.** It was 8,000 tokens (4,650 words
/// / 13,952 bytes) with a 6,000-word paste. Raising `MIN_BUDGET_BYTES` to
/// 50,000 floors any window under 26,024 tokens, so that route's pair became
/// (6,250, 50,000) and the paste no longer busted it — the non-vacuity check
/// below is what said so. 30,000 is the narrowest round window that is still
/// **not** floored, which keeps this test about a declared window's label
/// rather than about the floor's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_marker_names_the_routes_window_when_the_turn_is_seeded_by_carried_turn() {
    let fixture = Fixture::new("carryseed", "I answered the shortened version.");
    let budget = derive(BudgetInputs {
        window: 30_000,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });
    assert_eq!(budget.window_label, "kimi's context window");
    assert!(
        !budget.floored,
        "this route must be the declaration's own pair, not the floor's, or the \
         marker under test names a window nobody declared"
    );
    let config = HarnessConfig::default().with_route_budget(budget.clone());

    // Seeded exactly as `DaemonRuntime::run_prompt_turn` seeds a turn: the head
    // rebuilt from this turn's tools and route, the budgets and the window
    // label taken from `config` by `begin` itself.
    let sessions = SessionRegistry::new();
    let mut conversation = CarriedTurn::begin(
        &sessions,
        &fixture.session,
        build_system_prompt(&fixture.tools, &config),
        &config,
        Arc::new(SessionTaint::new()),
        Vec::new(),
        format!("review this paste:\n{}", filler(16_000)),
        std::collections::BTreeSet::new(),
        false,
        // No notes in this fixture, so a reroute has nothing to re-render.
        None,
    );
    // Non-vacuity: the pasted block really is over the route's byte budget, so
    // the clamp has to fire and a marker has to be written.
    assert!(
        conversation.ctx().estimated_bytes() > budget.budget_bytes,
        "{} bytes against {}",
        conversation.ctx().estimated_bytes(),
        budget.budget_bytes
    );

    let mut sub = fixture.bus.subscribe(1_024);
    fixture.turn(conversation.ctx_mut(), &config).await;
    let published = drain(&mut sub);
    conversation.abandon();

    let prompt = fixture.prompt(0);
    assert!(
        prompt.contains("kimi's context window"),
        "a turn seeded by `CarriedTurn::begin` must carry the route's window \
         label into the marker; nothing in this test stamps it by hand: {}",
        &prompt[..prompt.len().min(400)]
    );
    assert!(
        !prompt.contains("the local context window"),
        "…and must not fall back to the local engine's name on a remote route"
    );
    assert!(
        published
            .pressure
            .iter()
            .any(|p| p.newest_user_elided && p.bound == BudgetBound::Window),
        "the clamp landed on the user's own message and is announced: {:?}",
        published.pressure
    );
}

/// A guard on this file's own instrument: [`PressureReport`] is the value every
/// assertion above is a projection of, and `is_quiet` is what decides whether a
/// turn says anything at all.
#[test]
fn a_report_with_nothing_in_it_is_the_one_that_says_nothing() {
    assert!(PressureReport::default().is_quiet());
    assert!(!PressureReport {
        dropped_blocks: 1,
        ..PressureReport::default()
    }
    .is_quiet());
    assert!(!PressureReport {
        elided_bytes: 1,
        ..PressureReport::default()
    }
    .is_quiet());
}

// ---------------------------------------------------------------------------
// REQ-585 AC-16 — the four route shapes, and the silence a refusal keeps
// ---------------------------------------------------------------------------
//
// REQ-586 made the budget a property of the **route attempt**. REQ-585 BR-8 is
// what a skill turn does when it does not fit one: it is refused whole, before
// any model call, with a message naming the skill, its size, the budget and the
// bound — never elided into something the user did not invoke. That refusal is a
// decision about a *route*, so the shapes it has to be right for are route
// shapes, and the honest way to build one is the way a user builds one: a
// `[providers.capabilities] max_context` in a real config, read by a real
// daemon.
//
// Hence the harness below. The rest of this file drives the turn loop in
// process, which is the right instrument for "a report became an event"; it
// cannot express "this provider declares a 4,096-token window", because nothing
// in process reads a config. So these four legs spawn the shipped binary.
//
// | route | budget | expected |
// |---|---|---|
// | `max_context = 128000` | ≈250 KB | the skill expands and reaches the provider |
// | `max_context = 0` | the default pair, stated | refused, `bound: unknown window`, remedy named |
// | `max_context = 4096` | floored to (2,048 / 16 KiB) | refused, and the message says it was floored |
// | the local tier | (21,162 / 63,488 B) | refused, `bound: local engine` |
//
// **And the silence (BR-8c).** A refused turn emits **no** `context_pressure`
// event of any kind — not a drop, not an elision, not a "nothing was clamped"
// notice. That is asserted as a drained-and-empty listing on each of the three
// refusals, and it is non-vacuous in this file above all others: the tests at
// the top of it are the ones that show what a turn which *is* clamped publishes
// through the same channel.
//
// Stage A versus Stage B — which of the two measurements refused — is
// `tetond/tests/skill_turn.rs`'s, and so are the client-side bytes. What is
// here is the route matrix and the silence.

#[path = "e2e/harness.rs"]
mod daemon_harness;

use std::time::Duration;

use daemon_harness::{
    openai_turn, remote_provider_block, remote_provider_block_with_window, tier_block, Daemon,
    DaemonOptions, MockProvider, MockResponse, Workspace,
};

/// The marker the fixture skill's body carries, so "the expansion reached the
/// provider" is a claim about bytes rather than about a call count.
const BIG_SKILL_MARKER: &str = "BIG-SKILL-BODY-MARKER";

/// A ~40 KB skill body: comfortably inside a 128k window's ≈250 KB budget and
/// comfortably outside every other budget in the table above (the default pair
/// is 32 KiB, the floored pair 16 KiB).
///
/// Sized in **bytes**, because for a prose body the byte half of REQ-586's pair
/// binds first — the correction the REQ's own description records.
fn big_skill_body() -> String {
    let mut body = format!("{BIG_SKILL_MARKER}\n");
    // Past the local tier's 63,488-byte half with the system prompt's room to
    // spare, and under discovery's 128 KiB ceiling. (40,000 while the local
    // byte half was 32,768.)
    while body.len() < 90_000 {
        body.push_str("padding padding padding padding padding padding padding\n");
    }
    body
}

/// A workspace whose repo holds one project skill with the oversized body.
fn workspace_with_big_skill(tag: &str) -> Workspace {
    let ws = Workspace::new(tag);
    let dir = ws.repo.join(".claude/skills/big");
    std::fs::create_dir_all(&dir).expect("create the skill directory");
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\ndescription: the oversized skill\n---\n{}",
            big_skill_body()
        ),
    )
    .expect("write the skill file");
    ws
}

/// A deterministic machine: no probe may pick a model or spend the budget.
fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Every tier bound to `provider`, so whichever category the expansion
/// classifies to, the route is the one under test.
fn all_tiers(provider: &str) -> String {
    ["reflex", "scan", "build", "think"]
        .iter()
        .map(|tier| tier_block(tier, provider))
        .collect()
}

/// The `-32023` a skill refusal carries: no provider saw this turn, so it is not
/// REQ-586's `-32022`.
const SKILL_EXPANSION_TOO_LARGE: i64 =
    teton_protocol::jsonrpc::error_code::SKILL_EXPANSION_TOO_LARGE;

/// Invoke `/big` over the socket and return the full response.
fn invoke_big(client: &mut daemon_harness::Client, session: &str) -> serde_json::Value {
    client.call(
        "session/prompt",
        serde_json::json!({
            "session_id": session,
            "prompt": [],
            "skill": { "name": "big", "raw_arguments": "" },
        }),
    )
}

/// Assert that a refused turn published nothing about context pressure (BR-8c).
fn assert_pressure_silent(client: &daemon_harness::Client, leg: &str) {
    let pressure = client.events_named("context_pressure");
    assert!(
        pressure.is_empty(),
        "the {leg} refusal published a context_pressure event — a turn that was \
         refused was not clamped, and saying otherwise describes a turn that \
         never ran: {pressure:?}"
    );
}

/// **AC-16's positive route: a declared 128k window carries the whole
/// expansion, and nothing is clamped on the way.**
///
/// The control for the three refusals below, and the "assembled prompt" leg:
/// what the provider received is searched for the body's own marker, so a turn
/// that reached the wire carrying a *shortened* expansion would fail here rather
/// than pass as "it was sent".
#[test]
fn a_skill_that_fits_a_declared_window_reaches_the_provider_whole_and_clamps_nothing() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Read it all.",
            None,
            9_000,
            20,
        ))],
        MockResponse::ok(openai_turn("Done.", None, 10, 5)),
    );
    let ws = workspace_with_big_skill("skillfit-128k");
    let mut config = remote_provider_block_with_window(
        "frontier",
        &provider.openai_endpoint(),
        "claude-opus-4",
        128_000,
    );
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&all_tiers("frontier"));
    ws.write_config(&config);
    // A live local tier so the first-run consent gate is not in the way; every
    // turn here routes remotely.
    let script = ws.write_script("unused: this turn runs remotely");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let turn = invoke_big(&mut client, &session);
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "a skill inside its route's budget must run: {turn}\ndaemon log:\n{}",
        daemon.log()
    );

    // More than one request is expected and not asserted against: a session's
    // first turn also spends the title-naming attempt on the same text
    // (`skill_turn::a_skill_turn_spends_the_naming_attempt_on_the_expansion`),
    // and that one is a *duty* with a duty's own small budget. What matters is
    // that the turn itself carried the body whole, so the assertion is over the
    // request that carried the most of it.
    let requests = provider.requests();
    assert!(!requests.is_empty(), "nothing reached the provider at all");
    let bodies: Vec<String> = requests
        .iter()
        .map(|body| String::from_utf8_lossy(body).into_owned())
        .collect();
    let turn = bodies
        .iter()
        .max_by_key(|body| body.matches("padding padding").count())
        .expect("at least one request");
    assert!(
        turn.contains(BIG_SKILL_MARKER),
        "the provider never received the expansion"
    );
    assert!(
        turn.matches("padding padding").count() > 100,
        "the expansion reached the provider shortened — BR-8 carries a skill \
         whole or refuses it, and never in between; the fullest request held \
         {} padded lines",
        turn.matches("padding padding").count()
    );
    client.drain_events(Duration::from_millis(300));
    assert_pressure_silent(&client, "128k");
}

/// **AC-16's `max_context = 0` route.** A provider that declares no window gets
/// the default pair *stated* rather than silently assumed, so an oversized skill
/// is refused with `bound: unknown window` — and the message names the key the
/// user would go and write.
#[test]
fn an_undeclared_window_refuses_the_skill_and_names_the_key_to_set() {
    let provider = MockProvider::start(Vec::new(), MockResponse::ok(openai_turn("x", None, 1, 1)));
    let ws = workspace_with_big_skill("skillfit-unknown");
    // `max_context = 0` is the explicit "unknown", which is not the same
    // document as no capabilities table at all.
    let mut config = remote_provider_block_with_window(
        "frontier",
        &provider.openai_endpoint(),
        "claude-opus-4",
        0,
    );
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&all_tiers("frontier"));
    ws.write_config(&config);
    let script = ws.write_script("unused: this turn is refused before any call");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let turn = invoke_big(&mut client, &session);
    let message = turn["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        turn["error"]["code"].as_i64(),
        Some(SKILL_EXPANSION_TOO_LARGE),
        "an oversized skill on an unknown window must be refused: {turn}"
    );
    assert!(
        message.contains("`/big` does not fit this route's context budget"),
        "the refusal must name the skill: {message}"
    );
    assert!(
        message.contains(&format!("bound: {}", BudgetBound::DefaultUnknown.words())),
        "the bound must be spoken, never spelled the wire way: {message}"
    );
    assert!(
        message.contains("set `capabilities.max_context` for `frontier`"),
        "the bound a new user meets must carry its remedy: {message}"
    );
    assert_eq!(
        provider.request_count(),
        0,
        "nothing was sent and no provider saw this turn"
    );
    client.drain_events(Duration::from_millis(300));
    assert_pressure_silent(&client, "unknown-window");
}

/// **AC-16's floored route — the shipped Ollama recipe's shape.** A declared
/// `max_context = 4096` derives a pair below REQ-586's floor and is raised to
/// (2,048 / 16 KiB): a budget *larger* than the declaration allows. The refusal
/// says so, because the bound alone cannot.
#[test]
fn a_floored_window_refuses_the_skill_and_says_the_declaration_is_not_in_force() {
    let provider = MockProvider::start(Vec::new(), MockResponse::ok(openai_turn("x", None, 1, 1)));
    let ws = workspace_with_big_skill("skillfit-floored");
    let mut config = remote_provider_block_with_window(
        "frontier",
        &provider.openai_endpoint(),
        "claude-opus-4",
        4_096,
    );
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&all_tiers("frontier"));
    ws.write_config(&config);
    let script = ws.write_script("unused: this turn is refused before any call");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let turn = invoke_big(&mut client, &session);
    let message = turn["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        turn["error"]["code"].as_i64(),
        Some(SKILL_EXPANSION_TOO_LARGE),
        "an oversized skill on a floored window must be refused: {turn}"
    );
    assert!(
        message.contains("floored"),
        "a floored route must say the declared ceiling is not in force: {message}"
    );
    assert_eq!(provider.request_count(), 0, "no provider saw this turn");
    client.drain_events(Duration::from_millis(300));
    assert_pressure_silent(&client, "floored-window");
}

/// **AC-16's local route.** The tier with no declaration to read at all: the
/// budget is the local pair and the bound is spoken as `local engine` — the
/// sentence the dogfood runbook records (AC-20d).
#[test]
fn the_local_route_refuses_an_oversized_skill_and_names_the_local_engine() {
    let ws = workspace_with_big_skill("skillfit-local");
    let mut config = String::from("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&remote_provider_block(
        "frontier",
        "http://127.0.0.1:9/v1/chat/completions",
        "claude-opus-4",
    ));
    config.push_str(&all_tiers("local"));
    ws.write_config(&config);
    let script = ws.write_script("unused: this turn is refused before any call");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let turn = invoke_big(&mut client, &session);
    let message = turn["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        turn["error"]["code"].as_i64(),
        Some(SKILL_EXPANSION_TOO_LARGE),
        "an oversized skill on the local tier must be refused: {turn}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert!(
        message.contains(&format!("bound: {}", BudgetBound::LocalEngine.words())),
        "the local tier's bound must be spoken as `local engine`: {message}"
    );
    assert!(
        !message.contains("local_engine"),
        "the snake_case wire spelling must never reach a sentence a user reads: \
         {message}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_pressure_silent(&client, "local");
}
