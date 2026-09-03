//! REQ-567 acceptance: what a carried conversation costs at the cache, and
//! what it must never change (TASK-096).
//!
//! # Where each acceptance criterion lives
//!
//! The matrix is spread across three homes, because the evidence each AC asks
//! for lives at a different seam. This is the map; nothing in REQ-567 is
//! covered only by code inspection.
//!
//! | AC | Test | Where |
//! |----|------|-------|
//! | AC-1 recap | `a_third_prompt_carries_the_first_prompts_message_and_reply` | `runtime.rs` (`tests::conversation_carry`) |
//! | AC-2 privacy across prompts | `boundary_content_read_in_prompt_one_blocks_the_remote_prompt_two` | `tests/e2e/conversation_carry.rs` |
//! | AC-3 budget | [`a_session_driven_past_its_budget_compacts_and_keeps_answering`] | here |
//! | AC-4 serialization | `a_concurrent_prompt_is_refused_by_name_and_leaves_the_conversation_linear` | `runtime.rs` |
//! | AC-5 atomicity | `a_turn_that_fails_after_a_tool_call_leaves_no_trace_in_the_next_context` | `runtime.rs` |
//! | AC-6 clear | `a_clear_empties_the_conversation_and_announces_what_it_dropped`, `no_tool_can_clear_a_session_and_no_mcp_wiring_path_could` | `runtime.rs`, `harness/tools/mod.rs` |
//! | AC-7 A/B | [`the_cache_changes_no_assembled_context_and_no_output_across_prompts`] | here |
//! | AC-8 boundary warmth | [`every_well_behaved_boundary_reuses_the_whole_retained_conversation`] | here |
//! | AC-9 multi-client | `client_bs_prompt_carries_the_conversation_client_a_left_behind` | `tests/e2e/conversation_carry.rs` |
//! | AC-10 mutation check | executed by hand — see below | — |
//! | AC-11 duty-input bound | `the_classifiers_input_stays_fixed_while_the_conversation_grows` | `runtime.rs` |
//! | AC-12 cross-session isolation | `two_interleaved_sessions_never_see_each_others_conversations` | `runtime.rs` |
//! | REQ-586 AC-11 route change between turns | [`a_conversation_assembled_on_a_128k_route_survives_a_local_turns_smaller_budget`] | here |
//! | BR-4 tool-free budget | [`a_tool_free_session_is_measured_and_bounded_at_every_prompt`] | here |
//! | BR-3 dropped provenance | `a_truncated_away_boundary_read_still_pins_the_session_and_the_next_prompt` | `runtime.rs` |
//! | C-2 pin on abort | `a_cancelled_turn_that_read_boundary_content_leaves_the_session_pinned` | `runtime.rs` |
//!
//! AC-1's tool-free recap answering and AC-8's real-model leg are open items in
//! `docs/manual-verification.md` (REQ-567 section): no default build links a
//! model that can be asked to recap, and no scripted engine can answer one.
//!
//! # What this file can and cannot prove
//!
//! Every test here drives the **real** turn loop
//! ([`run_session_turn_with_source`]) over a scripted engine whose
//! `complete_cached` runs the **real** [`PrefixCacheState`] and the **real**
//! [`over_window`] guard — the policy-pure seam REQ-564's architecture D-2
//! exists for, and the one LESSON-499 requires an AC-8 assertion to consume
//! rather than to re-implement. It seeds and commits each prompt through
//! [`CarriedTurn`] — the **same type** `DaemonRuntime::run_prompt_turn` uses,
//! not a re-typing of what it does — so a change to the seed/commit protocol
//! cannot leave this file agreeing with a dispatch that no longer exists
//! (LESSON-451; see the revised mutation 1 below, and the fixture's own
//! `Carry::prompt`).
//!
//! What still lives in the in-crate module is what needs the crate-private
//! engine slot and config: AC-1/AC-11/AC-12's claims are about the **dispatch**
//! around the turn — routing, the classifier, the title duty — and only an
//! in-crate test can install a recording engine into a `DaemonRuntime`. The
//! claim here is about what the cache and the ledger do with a carried
//! conversation, and about the budget that bounds it.
//!
//! It does **not** prove that llama.cpp reuses the KV: no default build links
//! one (`prefix_cache_session.rs` says the same at greater length). The
//! evidence for that is the dogfood measurement in
//! `docs/manual-verification.md`.
//!
//! # AC-10: the mutation check, executed
//!
//! Run twice on 2026-08-10 against this branch, each mutation reverted
//! immediately after. No `#[cfg]` or feature flag was left behind — the point
//! is that these tests fail against the code as it was, not that the code can
//! be configured to fail.
//!
//! **Mutation 1 — the dispatch stops seeding.** Re-run 2026-08-10 against the
//! shared seam: the `ctx.replay(...)` line removed from [`CarriedTurn::begin`],
//! leaving literally the pre-REQ dispatch (`ContextManager::new`, then
//! `push_user`). Five in-crate tests, three of the four tests in **this file**,
//! and both e2e legs went red:
//!
//! - `a_third_prompt_carries_the_first_prompts_message_and_reply` (AC-1) —
//!   "prompt 2 lost prompt 1's message";
//! - `two_interleaved_sessions_never_see_each_others_conversations` (AC-12) —
//!   "turn 5 carried nothing at all, so it proves nothing about whose
//!   conversation it carried";
//! - `the_classifiers_input_stays_fixed_while_the_conversation_grows` (AC-11) —
//!   its non-vacuity leg: "the agent context must grow across the session, or
//!   the fixed classifier input is fixed against nothing: turn 2 was 3626 bytes
//!   against turn 1's 3626";
//! - `a_fabricated_continuation_never_enters_the_carried_conversation` (BR-1)
//!   — "the KEPT text is what carries";
//! - `a_cancelled_turn_commits_its_completed_work_and_not_the_pending_call`
//!   (OQ-1) — "the completed turn's prose was lost by the cancelled one";
//! - e2e `boundary_content_read_in_prompt_one_blocks_the_remote_prompt_two`
//!   (AC-2) — "expected exactly one privacy_block for the carried boundary
//!   content, got []";
//! - e2e `client_bs_prompt_carries_the_conversation_client_a_left_behind`
//!   (AC-9) — "client B's turn context is missing the message client A sent —
//!   B joined a session, not a conversation".
//!
//! …plus, here, [`every_well_behaved_boundary_reuses_the_whole_retained_conversation`],
//! [`a_session_driven_past_its_budget_compacts_and_keeps_answering`] and
//! [`a_tool_free_session_is_measured_and_bounded_at_every_prompt`].
//!
//! That last part is new, and it is the point of routing this fixture through
//! [`CarriedTurn`]. The first run of this mutation (before the verify pass) left
//! every test in this file green, because the fixture re-implemented the
//! seed/commit sequence by hand and therefore kept seeding after the dispatch
//! stopped — LESSON-451's shape exactly. There is now one implementation, so
//! there is nowhere for that divergence to live.
//!
//! **Mutation 2 — the store stops recording.** `SessionRegistry::
//! commit_conversation` returned before its write. The same end state by the
//! other route — every prompt replays an empty snapshot, so every prompt is a
//! fresh context — and it reddens this file too: nine in-crate tests, both e2e
//! legs, and, here:
//!
//! - [`every_well_behaved_boundary_reuses_the_whole_retained_conversation`]
//!   (AC-8) — "boundary 1 is a pure extension — the conversation was carried
//!   unchanged, so nothing compared can have disagreed". Reuse had collapsed to
//!   the system head, so the boundary came back a *divergent* hit: the
//!   2026-08-10 dogfood measurement, reproduced by removing the carry;
//! - [`a_session_driven_past_its_budget_compacts_and_keeps_answering`] (AC-3) —
//!   "prompt 2 opened on 463 tokens against prompt 1's 464 — it started from a
//!   bare head, so nothing here is about a budget that spans a session".
//!
//! **Mutation 3 — the text carries, the tag does not.** Not required by AC-10,
//! run because AC-2 deserves it: `ContextManager`'s replay put tool
//! blocks back through `push_model`, keeping every byte and dropping the
//! `Provenance::Tool` that names the file. The AC-2 leg went red as designed
//! ("expected exactly one privacy_block … got []") — and the suite-wide capture
//! then caught what that costs: `BR-1 VIOLATION: boundary secret leaked into
//! captured egress payload #1`. The unblocked turn put the fixture repo's
//! secret on the wire. Provenance survival is the whole of BR-3, and this is
//! what its absence looks like.
//!
//! AC-7 stayed green under all three, which is correct and worth stating: it
//! compares the cache against itself, so it is a cache-independence claim, not
//! a carry claim.
//!
//! # The verify pass's own mutations (2026-08-10)
//!
//! Four more, each reverted immediately, covering the fixes this file and the
//! in-crate module gained at verify time:
//!
//! - **the budget gate moved back to the tool-result fold** —
//!   [`a_tool_free_session_is_measured_and_bounded_at_every_prompt`] and
//!   [`a_session_driven_past_its_budget_compacts_and_keeps_answering`] red, plus
//!   `turn_loop`'s own `a_turn_whose_compact_duty_cannot_serve_still_ends_under_budget`;
//! - **`truncate_to_budget` stops absorbing the dropped block's provenance** —
//!   three `context` unit tests and the in-crate
//!   `a_truncated_away_boundary_read_still_pins_the_session_and_the_next_prompt`
//!   red;
//! - **the taint pin skips the cancellation commit** — the in-crate
//!   `a_cancelled_turn_that_read_boundary_content_leaves_the_session_pinned` red;
//! - **the OQ-1 trim is skipped** — the in-crate
//!   `a_cancelled_turn_commits_its_completed_work_and_not_the_pending_call` red
//!   (the version of that test *before* this pass stayed green under it: its
//!   `&& !t.contains("tool")` clause exempted exactly the dangling call);
//! - **the truncation flag stops carrying** —
//!   `a_commit_carries_the_truncation_note_and_the_dropped_provenance` red;
//! - **`Drop` stops checking `std::thread::panicking()`** —
//!   `a_panicking_turn_commits_nothing_while_an_aborted_one_commits` red.
//!
//! # REQ-586 verify (2026-08-19): the fixture commits the daemon's way
//!
//! [`Carry::prompt_under`] called `CarriedTurn::commit()` — the
//! **report-discarding** twin of the commit-and-publish pair
//! `DaemonRuntime::run_prompt_turn` runs. So the BR-10 commit seam this file's
//! AC-11 leg is named for was never exercised here at all, and the one event
//! that leg asserts came from the turn loop's own gate. It now runs
//! [`commit_and_publish`] — the daemon's **own** function, not a two-line copy
//! of it — which is the same LESSON-451 rule as mutation 1 above: a fixture
//! standing in for the dispatch has to run the dispatch's protocol, or the seam
//! it stands in for is untested. (The first version of this fix inlined the
//! publish and so reproduced the defect one level down: deleting the daemon's
//! call left this file green.)
//!
//! On an *ordinary* completed turn the commit's report really is quiet — the
//! loop gates both of its `Ok` exits (BUG-157) and nothing appends between that
//! gate and the commit — so AC-11's count stays 1, asserted below as "the
//! loop's gate, not twice over by the commit's". That is a fact about turns
//! whose context **fits**, and it is not the whole story:
//! `PressureReport::over_budget` is recomputed on every gate call and counts
//! toward `is_quiet()`, so a turn under a budget its own system prompt cannot
//! fit completes, reaches the commit, and the commit has something to say.
//! [`an_unfittable_turn_still_publishes_the_commits_own_report`] drives exactly
//! that, and `runtime.rs`'s
//! `the_commit_publishes_a_clamp_and_says_nothing_about_a_quiet_one` states the
//! decision directly.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use teton_inference::{
    over_window, ChatFormat, Completion, Engine, EngineError, GenParams, MissReason,
    PrefixCacheState,
};
use teton_protocol::events::{
    BudgetBound, ContextPressure, ContextPressureKind, Event, PrefixCacheOutcome,
};
use teton_protocol::{SessionId, SessionMode, TurnId};

use tetond::broadcast::EventBus;
use tetond::carry::CarriedTurn;
use tetond::cost::{CostLedger, LocalUsageMeter, NoopCostSink, PriceTable};
use tetond::harness::budget::derive;
use tetond::harness::compact::COMPACT_OUTPUT_CONTRACT;
use tetond::harness::context::{approx_tokens, APPROX_BYTES_PER_TOKEN};
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, BudgetInputs, DutyRoute, HarnessConfig,
    HarnessError, LocalEngineSource, NoopProvenanceHook, PendingPermissions, PermissionConfig,
    PermissionGate, SessionEvents, ToolContext, ToolDuties, ToolRegistry, TurnOutcome,
    COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TRIAGE_DUTY,
};
use tetond::runtime::{commit_and_publish, refit_for_reroute, SessionTaint};
use tetond::sessions::SessionRegistry;

/// A window wide enough for the real system prompt plus several carried turns.
/// The point of these tests is the reuse arithmetic, not the window guard —
/// which `prefix_cache_session.rs` pins on both paths.
///
/// Widened for REQ-586 AC-11, whose conversation is 30,000 words **by
/// requirement** and is assembled whole on a 128k route before the local turn
/// meets it. A ceiling raised can only turn a refusal into a served call, and no
/// test in this file expects one; the guard itself is still the real
/// [`over_window`], run on both arms.
const WIDE_N_CTX: u32 = 65_536;

/// The opening of the system head: what tells a **turn** prompt from a duty's.
///
/// Only a turn carries the head; the duties (`compact`, `digest`) build their
/// own fixed frames. A scripted engine that answered every prompt off one
/// script would spend a turn's reply on a duty and shift the fixture by one —
/// the failure mode `ScriptedFileEngine`'s own docs record having shipped
/// twice.
const SYSTEM_HEAD_OPENING: &str = "You are Teton Code";

/// A deterministic stand-in for BPE tokenization: word-level and stable.
///
/// Identical to `prefix_cache_session.rs`'s, and deliberately the same
/// whitespace granularity as [`approx_tokens`] — so a token count asserted here
/// is the same currency the harness budgets in, and "the reuse collapsed to the
/// system head" is expressible as an equality rather than a range.
fn tokenize(prompt: &str) -> Vec<i32> {
    prompt
        .split_whitespace()
        .map(|word| {
            // FNV-1a, truncated. Any stable hash works; this one is short enough
            // to read in a failure message.
            let mut hash: u32 = 2_166_136_261;
            for byte in word.as_bytes() {
                hash ^= u32::from(*byte);
                hash = hash.wrapping_mul(16_777_619);
            }
            (hash & 0x7fff_ffff) as i32
        })
        .collect()
}

/// What one served **turn** did, and what it was handed.
#[derive(Debug, Clone)]
struct CallRecord {
    /// The assembled context the engine received, verbatim (AC-7 compares these
    /// byte for byte).
    prompt: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: u32,
    processed_tokens: u32,
    miss: Option<MissReason>,
    divergent: bool,
}

impl CallRecord {
    /// What the KV holds after this call: the prompt **plus what was generated**.
    ///
    /// This is REQ-564's record semantics, and it is what AC-8's "the full
    /// retained prior context" means in token terms — the next boundary either
    /// reuses all of it or the conversation did not carry.
    fn resident(&self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// A scripted engine that caches prefixes exactly as the real one does.
///
/// The cache is the real [`PrefixCacheState`] and the window guard is the real
/// [`over_window`], so a disagreement between this and production is a
/// disagreement in the *mechanism* (llama.cpp KV truncation), never in the
/// *policy* (REQ-564 architecture D-2, LESSON-499).
struct CarryEngine {
    /// One reply per **turn** call, in order.
    replies: Vec<String>,
    calls: usize,
    cache: PrefixCacheState,
    log: Arc<Mutex<Vec<CallRecord>>>,
    /// The duty prompts this engine answered, so a test can assert a duty was
    /// genuinely *asked* rather than inferring it from what the context ended up
    /// looking like.
    duties: Arc<Mutex<Vec<String>>>,
    /// When false, `complete_cached` offers no reuse at all and every turn takes
    /// the cold path — the "cache disabled" arm of AC-7's A/B.
    caching: bool,
}

/// What a fixture keeps of one engine: the turns it served and the duties it
/// was asked.
type EngineLogs = (Arc<Mutex<Vec<CallRecord>>>, Arc<Mutex<Vec<String>>>);

impl CarryEngine {
    fn new(replies: &[&str], caching: bool) -> (Self, EngineLogs) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let duties = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                replies: replies.iter().map(|r| (*r).to_owned()).collect(),
                calls: 0,
                cache: PrefixCacheState::new(),
                log: Arc::clone(&log),
                duties: Arc::clone(&duties),
                caching,
            },
            (log, duties),
        )
    }

    fn next_reply(&mut self) -> String {
        let reply = self
            .replies
            .get(self.calls)
            .cloned()
            .unwrap_or_else(|| "Done.".to_owned());
        self.calls += 1;
        reply
    }

    /// Stream `reply` through `on_token`, honouring an early stop — the
    /// containment scanner needs a token stream or it cannot cut anything.
    fn stream(reply: &str, params: &GenParams, on_token: &mut dyn FnMut(&str) -> bool) -> String {
        let mut text = String::new();
        for (emitted, token) in reply.split_inclusive(' ').enumerate() {
            if u32::try_from(emitted).unwrap_or(u32::MAX) >= params.max_tokens {
                break;
            }
            let keep_going = on_token(token);
            text.push_str(token);
            if !keep_going {
                break;
            }
        }
        text
    }

    fn record(&self, prompt: &str, completion: &Completion) {
        self.log.lock().expect("log mutex").push(CallRecord {
            prompt: prompt.to_owned(),
            prompt_tokens: completion.prompt_tokens,
            completion_tokens: completion.completion_tokens,
            cached_tokens: completion.cached_tokens,
            processed_tokens: completion.processed_tokens(),
            miss: completion.cache_miss,
            divergent: completion.cache_divergent,
        });
    }
}

/// What a duty gets. Answered off the turn script: a duty is not a turn
/// (REQ-561 BR-10), and one that consumed a scripted reply would shift every
/// later turn in the fixture by one.
///
/// The `compact` answer is **well formed** rather than garbage, so AC-3 drives
/// the duty's real parse and its real whole-answer rule rather than only the
/// no-answer path. Whether the answer is then *applied* is REQ-561's call, not
/// this fixture's: an answer that would leave the context over budget is
/// rejected whole, which is what happens in AC-3 (one fold larger than the
/// whole budget cannot be compacted away) and why the deterministic gate is
/// what ends up rewriting that history.
fn duty_answer(prompt: &str) -> String {
    if prompt.contains(COMPACT_OUTPUT_CONTRACT) {
        "FORGET: 1\nSUMMARY: an earlier turn read the notes file.".to_owned()
    } else {
        "none".to_owned()
    }
}

impl Engine for CarryEngine {
    fn model_id(&self) -> &str {
        "scripted-local-3b"
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        assert!(
            !prompt.contains(SYSTEM_HEAD_OPENING),
            "a turn reached the cold path: the local source asks for reuse on \
             every agent turn (REQ-564), so this can only be a fixture that \
             built its own source wrongly"
        );
        self.duties
            .lock()
            .expect("duty log mutex")
            .push(prompt.to_owned());
        let answer = duty_answer(prompt);
        let prompt_tokens = u32::try_from(prompt.split_whitespace().count()).unwrap_or(u32::MAX);
        let completion_tokens = u32::try_from(answer.split_whitespace().count()).unwrap_or(0);
        let text = Self::stream(&answer, params, on_token);
        Ok(Completion::cold(text, prompt_tokens, completion_tokens))
    }

    fn complete_cached(
        &mut self,
        session: &str,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        let ids = tokenize(prompt);
        let prompt_tokens = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        // The same guard the real engine runs, ahead of the probe, on both arms
        // (REQ-564 BR-7): reuse changes how many tokens are decoded, never how
        // many must fit.
        if let Some(refusal) = over_window(prompt_tokens, WIDE_N_CTX, params.max_tokens) {
            return Err(refusal);
        }

        if !self.caching {
            // The disabled arm: byte-for-byte the cold path, including the
            // script position, so an A/B compares like with like.
            let reply = self.next_reply();
            let completion_tokens = u32::try_from(reply.split_whitespace().count()).unwrap_or(0);
            let text = Self::stream(&reply, params, on_token);
            let completion = Completion::cold(text, prompt_tokens, completion_tokens);
            self.record(prompt, &completion);
            return Ok(completion);
        }

        let decision = self.cache.probe(session, &ids);
        let cached_tokens = u32::try_from(decision.reused()).unwrap_or(u32::MAX);

        let reply = self.next_reply();
        let completion_tokens = u32::try_from(reply.split_whitespace().count()).unwrap_or(0);
        let text = Self::stream(&reply, params, on_token);

        // The resident prefix is prompt + generated, because that is what the
        // real KV holds after a turn.
        let mut resident = ids;
        resident.extend(tokenize(&reply));
        self.cache.record(session, resident);

        let completion = Completion {
            text,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_miss: decision.miss_reason(),
            cache_divergent: decision.divergent(),
        };
        self.record(prompt, &completion);
        Ok(completion)
    }

    fn evict_prefix_cache(&mut self, _reason: teton_inference::EvictionReason) {
        self.cache.evict();
    }
}

/// A session store, a scripted local tier, a ledger, and the four dispatch
/// steps that put a conversation between them.
struct Carry {
    engine: Arc<Mutex<dyn Engine>>,
    log: Arc<Mutex<Vec<CallRecord>>>,
    duties: Arc<Mutex<Vec<String>>>,
    ledger: CostLedger,
    bus: Arc<EventBus>,
    sessions: SessionRegistry,
    /// The session-taint set [`CarriedTurn`] pins into. Unused by these
    /// fixtures' assertions (they declare no boundaries) and present because
    /// the production seam takes one — which is the point of using it.
    taint: Arc<SessionTaint>,
    tools: ToolRegistry,
    tool_ctx: ToolContext,
    config: HarnessConfig,
    /// Where the built-in tools are jailed, so a scratch fixture can be removed.
    cwd: PathBuf,
}

impl Carry {
    /// A fixture with the harness's default budgets and a scratch working
    /// directory.
    fn new(replies: &[&str], caching: bool, tag: &str) -> Self {
        Self::with_budget(replies, caching, tag, None)
    }

    /// The same, with an explicit context budget in whitespace tokens — how
    /// AC-3 drives a session onto the compaction machinery without a 16k
    /// fixture.
    fn with_budget(
        replies: &[&str],
        caching: bool,
        tag: &str,
        budget_tokens: Option<usize>,
    ) -> Self {
        let (engine, (log, duties)) = CarryEngine::new(replies, caching);
        let cwd = std::env::temp_dir().join(format!(
            "teton-carry-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&cwd).expect("scratch dir");

        let tools = ToolRegistry::with_builtins();
        let mut config = HarnessConfig {
            // Greedy, so "the same script twice" is the whole of AC-7's
            // fixed-seed premise: nothing here samples.
            gen_params: GenParams {
                max_tokens: 128,
                temperature: 0.0,
            },
            ..HarnessConfig::default()
        }
        // A tool result enters context whole: these fixtures fill the budget
        // deliberately, and a digest duty condensing them first would be the
        // thing under test rather than compaction. Both currencies, in one
        // call — setting only the token threshold leaves the byte twin at its
        // default and digests the very results this fixture needs kept
        // (REQ-586 TASK-189 verification).
        .without_digest();
        if let Some(tokens) = budget_tokens {
            let system = build_system_prompt(&tools, &config);
            config.context_budget_tokens = approx_tokens(&system) + tokens;
            // The byte twin in the ratio the harness itself uses
            // (`HarnessConfig::default`, `ContextManager::new`), so this fixture
            // presses on the same two gates a real session does rather than on
            // one of them with the other set impossibly wide.
            config.context_budget_bytes = config.context_budget_tokens * APPROX_BYTES_PER_TOKEN;
        }

        Self {
            engine: Arc::new(Mutex::new(engine)),
            log,
            duties,
            ledger: CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
                .expect("in-memory ledger"),
            bus: Arc::new(EventBus::new()),
            sessions: SessionRegistry::new(),
            taint: Arc::new(SessionTaint::new()),
            tools,
            tool_ctx: ToolContext::new(&cwd),
            config,
            cwd,
        }
    }

    /// The system head this fixture's turns are assembled under.
    fn system(&self) -> String {
        build_system_prompt(&self.tools, &self.config)
    }

    fn session(&self) -> SessionId {
        self.sessions
            .create(SessionMode::Freeform, None, None)
            .expect("a freeform session needs no phase")
            .session_id
    }

    /// One prompt turn, through the **production** seed/claim/commit protocol.
    ///
    /// [`CarriedTurn`] is the type `DaemonRuntime::run_prompt_turn` uses, and it
    /// is used here rather than re-typed because LESSON-451 is exactly this
    /// file's failure mode: the fixture used to rebuild the head, replay a
    /// snapshot, push the message and commit the blocks by
    /// hand, all in the right order — which meant a dispatch that stopped
    /// seeding left every test in this file green (see the mutation notes in the
    /// module doc). With one implementation and two callers, that mutation
    /// reddens both.
    ///
    /// What remains local to the fixture is only what the daemon's dispatch does
    /// *around* the turn and this file is not asserting: routing, the tool
    /// registry, the classifier and the title duty.
    async fn prompt(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<TurnOutcome, HarnessError> {
        self.prompt_under(session_id, text, &self.config).await
    }

    /// The same turn, under a config that is not the fixture's own — how AC-11
    /// assembles a conversation on one route's budget and takes the next prompt
    /// on another's (REQ-586 BR-10).
    ///
    /// [`Carry::with_budget`] is per **fixture**; a budget that changes between
    /// turns is per **prompt**, and it has to be, because the pair
    /// [`CarriedTurn::begin`] seeds the manager from is the pair of the route
    /// *this* turn takes. A fixture that could only state one budget could
    /// express "a session that always had 4k" or "a session that always had
    /// 128k" and not the thing BR-10 is about: the same conversation meeting a
    /// smaller window on its next turn.
    ///
    /// The head is rebuilt from `config` for the same reason the daemon rebuilds
    /// it per turn (BR-7): the head is a function of the turn's tools and route,
    /// and seeding a replayed conversation under a head from a different one is
    /// the fossilization REQ-567 closed.
    async fn prompt_under(
        &self,
        session_id: &SessionId,
        text: &str,
        config: &HarnessConfig,
    ) -> Result<TurnOutcome, HarnessError> {
        let gate = PermissionGate::new(
            session_id.clone(),
            PermissionConfig::permissive(),
            Arc::clone(&self.bus),
            Arc::new(PendingPermissions::new()),
        );
        let events = SessionEvents::new(Arc::clone(&self.bus), session_id.clone());

        // The BR-5 claim, taken as dispatch takes it — first, and held until the
        // commit has landed. Declared before the turn so it outlives it (locals
        // drop in reverse).
        let _claim = self
            .sessions
            .try_begin_turn(session_id, &TurnId::from(format!("turn-{text:.8}")))
            .expect("the fixture drives one turn at a time");
        let mut conversation = CarriedTurn::begin(
            &self.sessions,
            session_id,
            build_system_prompt(&self.tools, config),
            config,
            Arc::clone(&self.taint),
            // No boundaries in these fixtures: the taint pin's own behaviour is
            // pinned by the runtime tests and by the AC-2 e2e leg, which are the
            // ones that can watch egress.
            Vec::new(),
            text,
            std::collections::BTreeSet::new(),
            false,
            // No notes in this fixture, so a reroute has nothing to re-render.
            None,
        );

        // Metered exactly as dispatch meters it, so the ledger rows below are
        // the projection the daemon writes rather than numbers this test made up.
        let mut source = LocalEngineSource::new(
            Arc::clone(&self.engine),
            ChatFormat::Flat,
            session_id.clone(),
        )
        .metered(Arc::new(self.ledger.clone()) as Arc<dyn LocalUsageMeter>);
        let digest = DutyRoute::local(DIGEST_DUTY, "local", Arc::clone(&self.engine));
        let compact = DutyRoute::local(COMPACT_DUTY, "local", Arc::clone(&self.engine));
        let triage = DutyRoute::local(TRIAGE_DUTY, "local", Arc::clone(&self.engine));
        let shell = DutyRoute::local(SHELL_DUTY, "local", Arc::clone(&self.engine));
        let mut hook = NoopProvenanceHook;

        let outcome = run_session_turn_with_source(
            &mut source,
            &self.tools,
            &self.tool_ctx,
            &gate,
            &events,
            conversation.ctx_mut(),
            config,
            &mut hook,
            &digest,
            &compact,
            &ToolDuties {
                triage: &triage,
                shell: &shell,
            },
        )
        .await;

        // Committed exactly as `DaemonRuntime::run_prompt_turn` commits — the
        // **reporting** variant, published through the daemon's own
        // `publish_commit_pressure` (REQ-586 BR-10, verify M7-b).
        //
        // `commit()` — the report-discarding twin — is the right default for a
        // caller with no event handle, and it is what this fixture used to
        // call. That made AC-11 below silently unable to see the commit seam at
        // all: `if false && !pressure.is_quiet()` at the daemon's own publish
        // left the whole `tetond` package green, because the one event AC-11
        // asserted came from the turn loop's gate and no fixture anywhere drove
        // the commit's.
        //
        // Calling the daemon's own function rather than re-typing its two
        // lines is the second half of that fix, and the same LESSON-451 rule
        // one level down: the first attempt inlined `if !pressure.is_quiet() {
        // events.context_pressure(..) }` here, which is a *copy* of the
        // dispatch's publish — so neutering the real one still left this file
        // green, and the fixture was testing itself. `commit_and_publish` is
        // the whole success-arm protocol, so this fixture and
        // `run_prompt_turn` cannot come to run different ones.
        if outcome.is_ok() {
            commit_and_publish(conversation, &events, &config.budget);
        } else {
            conversation.abandon();
        }
        outcome
    }

    /// Every turn this fixture's engine served, in order.
    fn calls(&self) -> Vec<CallRecord> {
        self.log.lock().expect("log mutex").clone()
    }

    /// How many times the `compact` duty was actually consulted.
    fn compact_duty_calls(&self) -> usize {
        self.duties
            .lock()
            .expect("duty log mutex")
            .iter()
            .filter(|prompt| prompt.contains(COMPACT_OUTPUT_CONTRACT))
            .count()
    }

    /// The tokens the session has retained, in the harness's own currency.
    fn retained_tokens(&self, session_id: &SessionId) -> usize {
        self.sessions
            .conversation_snapshot(session_id)
            .blocks()
            .iter()
            .map(|block| approx_tokens(&block.text))
            .sum()
    }

    fn retained_blocks(&self, session_id: &SessionId) -> usize {
        self.sessions.conversation_snapshot(session_id).len()
    }

    /// Everything the session has retained, as one string — so "the oldest
    /// paste is gone and the newest is still here" is a claim about content
    /// rather than about a block count that any rewrite would also satisfy.
    fn retained_text(&self, session_id: &SessionId) -> String {
        self.sessions
            .conversation_snapshot(session_id)
            .blocks()
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The bytes the session has retained, in the gate's own currency.
    fn retained_bytes(&self, session_id: &SessionId) -> usize {
        self.sessions
            .conversation_snapshot(session_id)
            .blocks()
            .iter()
            .map(|block| block.text.len())
            .sum()
    }
}

impl Drop for Carry {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

// ---------------------------------------------------------------------------
// AC-8 — the boundary is a pure extension of the whole conversation
// ---------------------------------------------------------------------------

/// **AC-8 / BR-1.** In a three-prompt session with caching on, every boundary
/// after the first is a `prefix_cache_hit` with `divergent: false` whose
/// `cached_tokens` equals **the full retained prior context** — the resident
/// prefix, prompt plus generated (REQ-564's record semantics) — and not the
/// system head.
///
/// The equality is the assertion, not a bound: "reuse grew" would pass against
/// a boundary that reused the head plus one carried block, which is the shape
/// carry is supposed to make impossible. The head-sized floor beneath it is the
/// 2026-08-10 dogfood measurement stated as a test: every boundary in that
/// session reused ~814 tokens, because the head was all consecutive prompts
/// shared.
///
/// The event and ledger legs ride the same run because BR-9's claim is that the
/// two agree: both are projections of one `Completion`, and a test that read
/// only the engine's own log would not notice a projection that dropped or
/// transposed them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_well_behaved_boundary_reuses_the_whole_retained_conversation() {
    let carry = Carry::new(
        &[
            "The router allows three attempts.",
            "Yes, the fallback shares that budget.",
            "We established the retry budget and the fallback.",
        ],
        true,
        "ac8",
    );
    let session = carry.session();
    let mut events = carry.bus.subscribe(64);

    for text in [
        "how many attempts does the router allow?",
        "does the fallback share it?",
        "recap what we established",
    ] {
        carry
            .prompt(&session, text)
            .await
            .expect("the scripted turn completes");
    }

    let calls = carry.calls();
    assert_eq!(calls.len(), 3, "one turn call per prompt: {}", calls.len());
    let head_tokens = u32::try_from(approx_tokens(&carry.system())).expect("head fits a u32");

    // The first prompt is necessarily cold — there was nothing resident.
    assert_eq!(calls[0].miss, Some(MissReason::Cold));
    assert_eq!(calls[0].cached_tokens, 0);

    for n in 1..calls.len() {
        let call = &calls[n];
        let previous = &calls[n - 1];
        assert_eq!(call.miss, None, "boundary {n} must be a hit");
        assert!(
            !call.divergent,
            "boundary {n} is a pure extension — the conversation was carried \
             unchanged, so nothing compared can have disagreed"
        );
        assert_eq!(
            call.cached_tokens,
            previous.resident(),
            "boundary {n} reused {} tokens, but the conversation it should have \
             reused is {} tokens long (prompt {} + generated {}). A boundary \
             that reuses less than the whole retained context is the \
             pre-carry shape: consecutive prompts sharing only what they both \
             rebuild.",
            call.cached_tokens,
            previous.resident(),
            previous.prompt_tokens,
            previous.completion_tokens
        );
        assert!(
            call.cached_tokens > head_tokens,
            "boundary {n} reused {} tokens against a {head_tokens}-token system \
             head — this is the 2026-08-10 dogfood measurement, in which every \
             boundary reused the head and nothing else",
            call.cached_tokens
        );
        assert_eq!(
            call.cached_tokens + call.processed_tokens,
            call.prompt_tokens,
            "cached and processed must partition the prompt exactly"
        );
    }

    // The event leg: one `prefix_cache` per turn, carrying the same split.
    let mut published = Vec::new();
    while let Ok(Some(envelope)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
    {
        if let Event::PrefixCache(cache) = envelope.event {
            assert_eq!(envelope.session_id.as_ref(), Some(&session));
            published.push(cache.outcome);
        }
    }
    assert_eq!(
        published.len(),
        calls.len(),
        "one prefix_cache event per local turn: {published:?}"
    );
    for n in 1..published.len() {
        match published[n] {
            PrefixCacheOutcome::Hit {
                cached_tokens,
                new_tokens,
                divergent,
            } => {
                assert_eq!(cached_tokens, u64::from(calls[n].cached_tokens));
                assert_eq!(new_tokens, u64::from(calls[n].processed_tokens));
                assert!(!divergent);
            }
            ref other => panic!("boundary {n} must be published as a hit, got {other:?}"),
        }
    }

    // The ledger leg (BR-9): one unpriced row per turn, whose cached count is
    // the same number the event carries.
    let rows = carry.ledger.all_records().expect("read the ledger");
    assert_eq!(rows.len(), calls.len(), "one row per local turn");
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row.provider_id, "local");
        assert_eq!(
            row.usd_micros, None,
            "local inference is usage, not spend — an unpriced row, never a \
             priced-at-zero one"
        );
        assert_eq!(row.input_tokens, u64::from(calls[n].prompt_tokens));
        assert_eq!(
            row.cached_tokens,
            Some(u64::from(calls[n].cached_tokens)),
            "row {n} disagrees with the event about what came for free"
        );
    }
    // What BR-9 exists for: the session's cached-vs-processed split is
    // derivable from the rows alone.
    let charged: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let free: u64 = rows.iter().filter_map(|r| r.cached_tokens).sum();
    assert!(
        free > charged / 2,
        "a carried session should be mostly reuse by the third prompt: {free} \
         of {charged} input tokens"
    );
}

// ---------------------------------------------------------------------------
// AC-7 — the cache is unobservable across boundaries
// ---------------------------------------------------------------------------

/// **AC-7 / BR-7.** A fixed-seed multi-prompt session produces byte-identical
/// assembled contexts and byte-identical outputs with the KV cache enabled and
/// disabled. Reuse is a latency property; carry does not make it anything else.
///
/// "Fixed seed" is met by construction here — the engine is scripted and
/// `temperature` is 0 — so the two runs differ in exactly one variable: whether
/// `complete_cached` offers reuse.
///
/// The assembled context is compared as well as the output because the output
/// alone would pass against a cache that changed what the model was shown and
/// got away with it on a fixture whose replies do not depend on the prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cache_changes_no_assembled_context_and_no_output_across_prompts() {
    const REPLIES: [&str; 3] = [
        "The parser handles nested objects.",
        "Yes, and arrays of them.",
        "We covered nesting and arrays.",
    ];
    const PROMPTS: [&str; 3] = [
        "what does the parser handle?",
        "does it handle arrays?",
        "recap what we covered",
    ];

    let mut answers = Vec::new();
    let mut contexts = Vec::new();
    let mut conversations = Vec::new();
    let mut reuse = Vec::new();
    for caching in [true, false] {
        let carry = Carry::new(
            &REPLIES,
            caching,
            if caching { "ac7-warm" } else { "ac7-cold" },
        );
        let session = carry.session();
        let mut arm = Vec::new();
        for text in PROMPTS {
            let outcome = carry
                .prompt(&session, text)
                .await
                .expect("the scripted turn completes");
            arm.push(outcome.final_text);
        }
        let calls = carry.calls();
        reuse.push(calls.iter().map(|c| c.cached_tokens).sum::<u32>());
        contexts.push(calls.into_iter().map(|c| c.prompt).collect::<Vec<_>>());
        conversations.push(
            carry
                .sessions
                .conversation_snapshot(&session)
                .blocks()
                .iter()
                .map(|block| (block.role, block.text.clone()))
                .collect::<Vec<_>>(),
        );
        answers.push(arm);
    }

    // Non-vacuity: the two arms really are the two arms. A run in which the
    // "cached" side reused nothing would compare a cold path against itself.
    assert!(
        reuse[0] > 0,
        "the cache-enabled arm reused nothing, so this A/B compared two cold runs"
    );
    assert_eq!(reuse[1], 0, "the cache-disabled arm must reuse nothing");

    assert_eq!(
        contexts[0], contexts[1],
        "prefix reuse changed the assembled context across a prompt boundary — \
         it must be observable only in latency (BR-7)"
    );
    assert_eq!(
        answers[0], answers[1],
        "prefix reuse changed the answers across a prompt boundary"
    );
    assert_eq!(
        conversations[0], conversations[1],
        "the conversation the session retained depends on the cache state"
    );
}

// ---------------------------------------------------------------------------
// AC-3 — the budget spans the session
// ---------------------------------------------------------------------------

/// **AC-3 / BR-4.** A session driven past the context budget across several
/// prompts compacts rather than fails: later prompts still succeed, no
/// over-window error escapes, and the boundary after a turn that rewrote its
/// history is a `divergent: true` hit reusing the common head — loudly, never
/// silently (REQ-564 AC-3's shape, now across prompts).
///
/// Why a divergent boundary is the *correct* answer here rather than a
/// regression: a turn that truncates rewrites the **oldest** end of the
/// conversation, which is the end the prefix cache reuses from. The next prompt
/// renders the same harness-authored preamble — system head plus the
/// `[earlier conversation truncated]` note, which is carried with the
/// conversation rather than re-derived per turn — and then a first block that is
/// no longer the first block the previous prompt had. REQ-564's amended BR-2
/// reuses exactly up to that disagreement. That is the "compacted boundary may
/// legitimately reuse less KV than the prior turn's prefix" case the spec calls
/// out, and it shows up in the telemetry rather than as a silent full-prompt
/// prefill.
///
/// Which of the two machineries does the rewriting is deliberately not asserted:
/// the `compact` duty is asked on every fold here (and the fixture checks that
/// it was), answers well-formed, and has its answer **rejected** as one that
/// would still leave the context over budget — a single fold larger than the
/// whole budget cannot be compacted away — so the unconditional
/// `truncate_to_budget` underneath it is what ends up rewriting the history.
/// That is REQ-561 ADR-4 working: the budget was never the duty's to enforce,
/// and BR-4's promise is that the session degrades to compaction rather than to
/// a failed turn, however the blocks get chosen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_driven_past_its_budget_compacts_and_keeps_answering() {
    // Two model calls per prompt: read the file, then answer. The file is large
    // enough that one fold busts a deliberately small budget.
    let read_notes = r#"{"tool": "read", "arguments": {"path": "notes.txt"}}"#;
    let carry = Carry::with_budget(
        &[
            read_notes,
            "The notes list the retry budget.",
            read_notes,
            "They also list the fallback rules.",
            read_notes,
            "Still the same notes.",
            read_notes,
            "Nothing new since.",
        ],
        true,
        "ac3",
        Some(400),
    );
    std::fs::write(
        carry.cwd.join("notes.txt"),
        (0..400)
            .map(|n| format!("line {n}: the retry budget is three attempts\n"))
            .collect::<String>(),
    )
    .expect("write the fixture file");

    let session = carry.session();
    let mut boundaries = Vec::new();
    let mut retained = Vec::new();
    for (n, text) in [
        "what do the notes say?",
        "and about the fallback?",
        "check the notes again",
        "anything new?",
    ]
    .into_iter()
    .enumerate()
    {
        let before = carry.calls().len();
        carry
            .prompt(&session, text)
            .await
            .unwrap_or_else(|err| panic!("prompt {} must still be served: {err}", n + 1));
        // The first call of each prompt IS the boundary: the context it was
        // handed is the carried conversation plus the new message.
        boundaries.push(carry.calls()[before].clone());
        retained.push((
            carry.retained_blocks(&session),
            carry.retained_tokens(&session),
        ));
    }

    // Non-vacuity: the session really did press on its budget. Without this the
    // assertions below would hold trivially on a session that never grew.
    let budget = carry.config.context_budget_tokens;
    let head = approx_tokens(&carry.system());
    assert!(
        retained
            .iter()
            .any(|(_, tokens)| head + tokens >= budget / 2),
        "the session never reached its budget ({retained:?} against {budget}), \
         so nothing here is about compaction"
    );
    // And the thing the budget exists to bound — the prompt the engine is
    // handed — stayed bounded on **every** call, four prompts deep. This is the
    // BR-4 claim in the currency that matters: the assembled context is what
    // has to fit, and carry is what could have made it grow without limit.
    //
    // Measured in bytes, because bytes are what the gate holds exactly: the
    // token heuristic's floor is one block (`truncate_to_budget` never drops the
    // most recent), so a single dense fold can sit above the token budget while
    // the byte gate clamps it in place — the deliberate asymmetry LESSON-446 is
    // about. The slack covers the rendered frame the estimate charges as a flat
    // per-block reserve.
    const FRAME_SLACK_BYTES: usize = 512;
    let byte_budget = carry.config.context_budget_bytes;
    for (n, call) in carry.calls().iter().enumerate() {
        assert!(
            call.prompt.len() <= byte_budget + FRAME_SLACK_BYTES,
            "call {} assembled a {}-byte prompt against a {byte_budget}-byte \
             budget — the budget bounded the turn but not the session",
            n + 1,
            call.prompt.len()
        );
    }
    // History was genuinely rewritten rather than merely appended to: a session
    // that only grew would show a monotonically rising block count.
    assert!(
        retained.windows(2).any(|w| w[1].0 <= w[0].0),
        "the conversation only ever grew ({retained:?}) — nothing compacted, so \
         the boundary assertions below are about an unpressured session"
    );
    // And the compaction machinery genuinely ran: the duty was consulted, not
    // bypassed by a fold that never crossed the soft threshold.
    assert!(
        carry.compact_duty_calls() > 0,
        "the `compact` duty was never asked, so this session pressed on the hard \
         gate without ever reaching the soft one"
    );

    // The budget is the *session's*, which presupposes that the later prompts
    // open on a conversation at all: each boundary after the first must be
    // handed more than a system head and a fresh message, or this fixture is
    // four unrelated turns that each happen to fill a budget.
    for (n, boundary) in boundaries.iter().enumerate().skip(1) {
        assert!(
            boundary.prompt_tokens > boundaries[0].prompt_tokens,
            "prompt {} opened on {} tokens against prompt 1's {} — it started \
             from a bare head, so nothing here is about a budget that spans a \
             session",
            n + 1,
            boundary.prompt_tokens,
            boundaries[0].prompt_tokens
        );
    }

    let head_tokens = u32::try_from(head).expect("head fits a u32");
    assert_eq!(
        boundaries[0].miss,
        Some(MissReason::Cold),
        "prompt 1 is cold"
    );
    for (n, boundary) in boundaries.iter().enumerate().skip(1) {
        assert_eq!(
            boundary.miss,
            None,
            "boundary {} must still be a hit: the system head is common to \
             every prompt, so a rewritten tail caps reuse rather than \
             destroying it",
            n + 1
        );
        assert!(
            boundary.divergent,
            "boundary {} followed a turn that rewrote its history, and a \
             rewrite that is not marked is exactly the silent compaction BR-4 \
             forbids",
            n + 1
        );
        // What was reused, read as text rather than as a number: `tokenize` is
        // whitespace-word granular, so the first `cached_tokens` words of this
        // prompt ARE the reused span. A rewritten history means it covers the
        // harness-authored preamble — head plus the truncation note the
        // conversation carries with it — and stops before any conversation
        // content. Asserted this way rather than against a token count because
        // the count is arithmetic over two constants nobody would notice
        // drifting; "no carried block was reused" is the claim.
        let reused: Vec<&str> = boundary
            .prompt
            .split_whitespace()
            .take(boundary.cached_tokens as usize)
            .collect();
        let reused = reused.join(" ");
        assert!(
            boundary.cached_tokens >= head_tokens,
            "boundary {} reused {} tokens against a {head_tokens}-token system \
             head: every prompt in a session shares at least its head",
            n + 1,
            boundary.cached_tokens
        );
        assert!(
            reused.contains("[earlier conversation truncated"),
            "boundary {} did not reuse the truncation note, so the note is being \
             re-derived per turn rather than carried with the conversation — \
             which also means it vanishes from the prompt on the next \
             untruncated turn",
            n + 1
        );
        assert!(
            !reused.contains("the retry budget is three attempts")
                && !reused.contains("what do the notes say?"),
            "boundary {} reused conversation content across a rewritten history: \
             the prompts must disagree at the first block",
            n + 1
        );
        assert_eq!(
            boundary.cached_tokens + boundary.processed_tokens,
            boundary.prompt_tokens,
            "the rewritten tail must be re-prefilled in full"
        );
    }
}

// ---------------------------------------------------------------------------
// BR-4 — the gate is on the carried path, not only on the tool-result fold
// ---------------------------------------------------------------------------

/// **BR-4, the tool-free session.** A conversation that never calls a tool is
/// still measured, still compacted, and still bounded — every prompt, from the
/// first model call onward.
///
/// ## What this is about
///
/// The budget gate — `compact_if_pressured` then the unconditional
/// `truncate_to_budget` — used to sit at exactly one place in the turn loop: the
/// tool-result fold. Every other path reached `prepare()` unmeasured, and the
/// first model call of a turn *always* does. For a session whose turns are
/// question-and-answer, that meant nothing measured the context at all: it grew
/// by a user block and a model block per prompt, was committed whole, replayed
/// whole into the next prompt, and grew again.
///
/// The failure that produces is not a slow degradation, it is a wedge. Once the
/// rendered prompt crosses the engine window the turn is refused with the typed
/// over-window error (REQ-564 BR-7) — and a refused turn **never commits**
/// (BR-6), so the oversized conversation stays exactly as it was and the next
/// prompt replays it into the same refusal. The session is dead for the rest of
/// the daemon's life, with no command that recovers it short of `/clear`. BR-4's
/// "a long-lived session degrades to compaction, never to a failed turn" is
/// precisely this.
///
/// ## What is asserted
///
/// That every prompt is served; that **every** assembled prompt the engine
/// received stayed inside the byte budget, which is the property the gate exists
/// to hold and the one that fails without it; and that the shrinking is
/// *observable* — the `compact` duty is genuinely consulted, the conversation is
/// genuinely rewritten rather than only appended to, and the assembled prompt
/// carries the honesty note that says history is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_free_session_is_measured_and_bounded_at_every_prompt() {
    // Long prose answers, no tool calls anywhere: this session's whole growth is
    // the conversation itself.
    let replies: Vec<String> = (0..6)
        .map(|n| {
            let body = (0..110)
                .map(|w| format!("answer{n}word{w} "))
                .collect::<String>();
            format!("Here is the {n}th explanation. {body}")
        })
        .collect();
    let reply_refs: Vec<&str> = replies.iter().map(String::as_str).collect();
    let carry = Carry::with_budget(&reply_refs, true, "budget-toolfree", Some(400));
    let session = carry.session();

    let prompts = [
        "explain the router",
        "and the fallback rules",
        "what about the retry budget",
        "how does the classifier fit in",
        "and the digest duty",
        "recap all of that",
    ];
    let mut retained = Vec::new();
    for (n, text) in prompts.into_iter().enumerate() {
        carry
            .prompt(&session, text)
            .await
            .unwrap_or_else(|err| panic!("prompt {} must still be served: {err}", n + 1));
        retained.push(carry.retained_blocks(&session));
    }

    let calls = carry.calls();
    assert_eq!(
        calls.len(),
        prompts.len(),
        "one turn call per prompt — a tool-free turn ends on its first reply"
    );

    // Non-vacuity: the session really did press on its budget. A fixture whose
    // conversation stayed small would satisfy every assertion below by never
    // having tested anything.
    let budget = carry.config.context_budget_tokens;
    let head = approx_tokens(&carry.system());
    assert!(
        carry.retained_tokens(&session) + head >= budget / 2,
        "the session never approached its budget ({} retained tokens against \
         {budget}), so nothing here is about a gate",
        carry.retained_tokens(&session)
    );

    // The claim. Measured in bytes for the reason AC-3 measures in bytes: the
    // byte gate is the one that holds exactly, and the slack covers the rendered
    // frame the estimate charges as a flat per-block reserve.
    const FRAME_SLACK_BYTES: usize = 512;
    let byte_budget = carry.config.context_budget_bytes;
    for (n, call) in calls.iter().enumerate() {
        assert!(
            call.prompt.len() <= byte_budget + FRAME_SLACK_BYTES,
            "prompt {} assembled a {}-byte context against a {byte_budget}-byte \
             budget — a tool-free session was never measured, so it grew until \
             the window refused it and the refusal wedged the session",
            n + 1,
            call.prompt.len()
        );
    }

    // And the shrinking was loud, not silent (BR-4).
    assert!(
        carry.compact_duty_calls() > 0,
        "the `compact` duty was never asked, so this session reached the hard \
         gate without ever reaching the soft one"
    );
    assert!(
        retained.windows(2).any(|w| w[1] <= w[0]),
        "the conversation only ever grew ({retained:?}) — nothing was ever \
         forgotten, so the bound above came from somewhere other than the gate"
    );
    assert!(
        calls
            .last()
            .expect("at least one call")
            .prompt
            .contains("[earlier conversation truncated"),
        "the last prompt does not say that history is missing — a conversation \
         cut without a word is the silent degradation BR-4 forbids"
    );
}

// ---------------------------------------------------------------------------
// REQ-586 AC-11 — a route change between turns is a budget change, and the
// carry survives it
// ---------------------------------------------------------------------------

/// **REQ-586 AC-11 / BR-10.** A session carries a 30,000-word conversation
/// assembled on a 128k route; the next turn routes local. The retained blocks
/// replay, the oldest are dropped to fit the smaller pair with a
/// `context_pressure` event saying so, the turn completes, and the session's
/// retained conversation afterwards is exactly what that local turn kept
/// (REQ-567 BR-6's atomic commit).
///
/// ## Why this needs a per-prompt budget and could not be written before
///
/// [`Carry::with_budget`] states one budget for a whole fixture, which can
/// express a session that always had 4k or one that always had 128k — never the
/// thing BR-10 is about. [`Carry::prompt_under`] exists for this test: the
/// twelve pasting turns run under a config carrying `derive`'s 128k pair, the
/// thirteenth runs under the fixture's default (local) one, and
/// [`CarriedTurn::begin`] seeds each turn's manager from the pair of the route
/// *that* turn takes — which is exactly what the daemon does.
///
/// ## The shape, and its bytes per word
///
/// Twelve pasted documents of 2,500 whitespace words at **4 bytes per word**
/// (`"abc "`), so the conversation is 30,000 words / ≈120 KB. On the 128k pair
/// (84,650 words / 253,952 bytes) that is under *both* soft thresholds — asserted
/// below, because a fixture that compacted on the way up would be pressing the
/// same machinery twice and proving neither. On the local pair (4,096 / 32,768)
/// one document plus the system head is what fits, which is why the paste is
/// 2,500 words rather than 5,000: the point is that a carried block **survives**
/// the drop, and a fixture that lost every one of them could not tell "the
/// oldest were dropped to fit" from "the session was cleared".
///
/// ## What the last assertion is for
///
/// "The retained conversation is what the local turn kept" is BR-6's atomic
/// commit read from the outside: the session ends up holding the manager's
/// surviving blocks plus the new exchange, not the pre-turn vector and not the
/// pre-drop one. Asserted as content — the twelfth paste's marker present, the
/// first paste's marker gone — because a block count alone is satisfied by any
/// rewrite that happens to arrive at the same length.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_assembled_on_a_128k_route_survives_a_local_turns_smaller_budget() {
    /// Whitespace words per pasted document, at 4 B/word.
    const PASTE_WORDS: usize = 2_500;
    /// How many of them the session accumulates: 12 × 2,500 = 30,000 words.
    const PASTES: usize = 12;

    let replies: Vec<&str> = vec!["Noted; the paste is in context."; PASTES + 1];
    let carry = Carry::new(&replies, true, "req586-ac11");
    let local = carry.config.clone();
    let remote_budget = derive(BudgetInputs {
        window: 128_000,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });
    assert_eq!(remote_budget.bound, BudgetBound::Window);
    // `without_digest` **after** `with_route_budget`: the latter sets both
    // thresholds from the route, so a fixture that wanted its pastes whole and
    // said so first would have them digested anyway (REQ-586 TASK-189).
    let remote = local
        .clone()
        .with_route_budget(remote_budget.clone())
        .without_digest();

    // The premise the fixture's arithmetic rests on, stated rather than assumed:
    // one pasted document has to fit the local pair beside the system head, or
    // nothing carried survives and this is a test about a cleared session.
    let head_tokens = approx_tokens(&carry.system());
    assert!(
        head_tokens + PASTE_WORDS + 64 < local.context_budget_tokens,
        "a {head_tokens}-word head leaves no room for a {PASTE_WORDS}-word \
         document under a {}-word budget — shrink the paste or this test proves \
         only that everything was thrown away",
        local.context_budget_tokens
    );

    let session = carry.session();
    let mut events = carry.bus.subscribe(4_096);

    // --- The conversation, assembled on the 128k pair. ---
    for n in 0..PASTES {
        let mut text = format!("document-{n}-marker ");
        for _ in 0..PASTE_WORDS {
            text.push_str("abc ");
        }
        carry
            .prompt_under(&session, &text, &remote)
            .await
            .unwrap_or_else(|err| panic!("remote paste {n} must be served: {err}"));
    }
    let carried_blocks = carry.retained_blocks(&session);
    let carried_tokens = carry.retained_tokens(&session);
    assert_eq!(
        carried_blocks,
        PASTES * 2,
        "one user block and one model block per paste, all retained"
    );
    assert!(
        carried_tokens >= 30_000,
        "AC-11 asks for a 30,000-word conversation; this one is {carried_tokens}"
    );
    // Nothing was clamped on the way up: the 128k pair had room for all of it,
    // which is what makes the drop below attributable to the *route change*.
    let climb = drain_pressure(&mut events);
    assert!(
        climb.is_empty(),
        "the conversation fitted the 128k pair, so nothing should have been \
         clamped assembling it: {climb:?}"
    );
    assert!(
        carried_tokens < remote_budget.budget_tokens * 70 / 100
            && carry.retained_bytes(&session) < remote_budget.budget_bytes * 70 / 100,
        "the remote leg must stay under the soft threshold too, or the fixture \
         compacts on the way up and the drop below is not the route change: \
         {carried_tokens} words / {} bytes",
        carry.retained_bytes(&session)
    );

    // --- The next turn routes local: the same conversation, a quarter of the
    // words and an eighth of the bytes. ---
    carry
        .prompt_under(&session, "what did the first document say?", &local)
        .await
        .expect("a session that outgrew its new route compacts, it does not fail");

    let pressure = drain_pressure(&mut events);
    // Exactly one, and it is the **loop's** gate rather than the commit's.
    //
    // Both are live in this fixture: `prompt_under` commits the daemon's way
    // (`commit_reporting`, published through the turn's `SessionEvents`), so a
    // commit that clamped anything would show up here as a second event. It
    // does not, and that is the shape rather than an accident — the loop gates
    // both of its exits (BUG-157), so a turn that *completed* arrives at the
    // commit already fitting. The commit gate is the backstop for the
    // cancellation paths, which reach it through `Drop` and publish nothing;
    // `runtime.rs::the_commit_publishes_a_clamp_and_says_nothing_about_a_quiet_one`
    // is what states its rule, because no completed turn can.
    assert_eq!(
        pressure.len(),
        1,
        "one clamp on the way down, announced once — and by the loop's gate, \
         not twice over by the commit's: {pressure:?}"
    );
    let event = &pressure[0];
    assert_eq!(event.kind, ContextPressureKind::BlocksDropped);
    assert_eq!(
        event.budget_tokens, local.context_budget_tokens as u64,
        "the event carries the budget the turn actually ran under, which is the \
         local one — the route changed, and so did the number"
    );
    assert_eq!(event.bound, BudgetBound::LocalEngine);

    // The seeded manager held the carried blocks plus the new message; what it
    // kept is what the session now holds, plus this turn's reply.
    let seeded = carried_blocks + 1;
    let kept = seeded - event.dropped_blocks as usize;
    assert!(
        kept >= 3,
        "a drop that kept only the new message would make 'the carry survived' \
         vacuous: {kept} of {seeded}"
    );
    assert_eq!(
        carry.retained_blocks(&session),
        kept + 1,
        "REQ-567 BR-6: the session holds what the turn kept plus what it added, \
         not the pre-turn vector and not the pre-drop one"
    );

    // …and it holds the *newest* end of the conversation, not the oldest.
    let retained = carry.retained_text(&session);
    assert!(
        retained.contains(&format!("document-{}-marker", PASTES - 1)),
        "the newest carried document must have survived the drop"
    );
    assert!(
        !retained.contains("document-0-marker"),
        "the oldest document must be gone — `truncate_to_budget` drops \
         oldest-first, and a retained conversation still holding it means the \
         commit wrote the pre-drop vector"
    );
    // And what it holds fits the route that kept it, in both currencies.
    assert!(
        head_tokens + carry.retained_tokens(&session) <= local.context_budget_tokens,
        "the retained conversation must fit the budget that produced it: {} + {} \
         against {}",
        head_tokens,
        carry.retained_tokens(&session),
        local.context_budget_tokens
    );
    assert!(
        carry.retained_bytes(&session) <= local.context_budget_bytes,
        "…in bytes too: {} against {}",
        carry.retained_bytes(&session),
        local.context_budget_bytes
    );

    // Non-vacuity for the machinery: the soft threshold was reached on the way
    // down, so this session degraded to compaction-then-truncation rather than
    // straight to the hard gate.
    assert!(
        carry.compact_duty_calls() > 0,
        "the `compact` duty was never asked on the local turn"
    );
    // And the turn the engine actually served fits the local budget.
    let last = carry.calls().last().expect("a local turn ran").clone();
    assert!(
        last.prompt.len() <= local.context_budget_bytes + 512,
        "the local turn assembled {} bytes against a {}-byte budget",
        last.prompt.len(),
        local.context_budget_bytes
    );
    assert!(
        last.prompt.contains("[earlier conversation truncated"),
        "the model must be told that history is missing"
    );
}

/// **REQ-586 BR-10, verify.** A turn that **completed** and could not fit its
/// budget reaches the commit's own gate with something to say, and the commit
/// says it.
///
/// This is the test `publish_commit_pressure` used to document as impossible.
/// The reasoning was sound when it was written and stopped being true at
/// TASK-194: the loop gates both of its `Ok` exits, so a completed turn never
/// arrives at the commit having *dropped or elided* anything — but
/// `PressureReport::over_budget` is recomputed on every gate call and counts
/// toward `is_quiet()`, and a context the gate could not fit is not made
/// fittable by gating it again. A budget below the harness's own system prompt
/// is the shape that produces it, and it is not a contrivance: it is exactly
/// what `budget::MIN_BUDGET_BYTES` exists to keep a *route* from ever deriving.
///
/// What the "impossible" claim cost is the point of writing this down. The
/// branch was documented as unreachable on the success path and therefore
/// guarded only by a unit test over the function itself, so deleting the
/// daemon's **call** to it — and with it every between-turns clamp report on a
/// live over-budget session — left the `tetond` package green.
///
/// Two lines go out for this turn, and the count is the assertion:
/// the loop's own over-budget latch (`turn_loop::announce_pressure`) bounds its
/// three gates to **one** `did_not_fit`, so the second can only be the
/// commit's. Deleting the publish takes it back to one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfittable_turn_still_publishes_the_commits_own_report() {
    let carry = Carry::new(&["Answered."], true, "req586-commit-didnotfit");

    // A pair no context can meet: the system head alone is several KB, so every
    // gate — the loop's two and the commit's one — finishes over budget. Built
    // by hand rather than through `derive`, because `derive`'s floor exists
    // precisely to stop a *route* from being budgeted this way (M1).
    let mut unfittable = derive(BudgetInputs::local());
    unfittable.budget_tokens = 200;
    unfittable.budget_bytes = 1_000;
    let config = carry
        .config
        .clone()
        .with_route_budget(unfittable.clone())
        .without_digest();
    assert!(
        carry.system().len() > config.context_budget_bytes,
        "non-vacuity: the system head ({} bytes) must not fit the {}-byte \
         budget, or nothing here is over budget",
        carry.system().len(),
        config.context_budget_bytes
    );

    let session = carry.session();
    let mut events = carry.bus.subscribe(256);
    carry
        .prompt_under(&session, "a question that will not fit", &config)
        .await
        .expect("a turn that cannot fit its budget is still served");

    let published = drain_pressure(&mut events);
    let did_not_fit: Vec<&ContextPressure> = published
        .iter()
        .filter(|p| p.kind == ContextPressureKind::DidNotFit)
        .collect();
    assert_eq!(
        did_not_fit.len(),
        2,
        "the loop says it once and the commit says it once — one of these is \
         the seam `publish_commit_pressure` is: {published:#?}"
    );
    for event in &did_not_fit {
        assert_eq!(event.budget_tokens, unfittable.budget_tokens as u64);
        assert_eq!(event.budget_bytes, unfittable.budget_bytes as u64);
        assert_eq!(event.bound, unfittable.bound);
    }
    // The commit still *committed* — the report is a by-product of the write,
    // never a replacement for it.
    assert!(
        !carry.retained_text(&session).is_empty(),
        "the turn's work was stored"
    );
}

/// Every `context_pressure` queued on `events` right now, oldest first.
///
/// `try_recv` rather than a timed `recv`: `EventBus::publish` is synchronous and
/// the turns above have already returned, so everything they published is
/// already queued — a wall-clock window is the assertion shape that goes flaky
/// first under CI scheduler pressure (LESSON-450).
fn drain_pressure(events: &mut tetond::broadcast::Subscription) -> Vec<ContextPressure> {
    let mut out = Vec::new();
    while let Some(envelope) = events.try_recv() {
        if let Event::ContextPressure(pressure) = envelope.event {
            out.push(pressure);
        }
    }
    assert!(
        !events.is_lagged(),
        "the subscription was evicted for falling behind, so an absent event \
         here would prove nothing"
    );
    out
}

// ---------------------------------------------------------------------------
// REQ-612 BR-3 verify — a reroute re-renders the notes at the route it lands on
// ---------------------------------------------------------------------------

/// **REQ-612 BR-3 (verify).** A turn assembled on a route with 8,192 bytes of
/// room for repository notes, rerouted to a floored one with 4,096, ends with a
/// 4,096-byte block **and still carrying the user's own message**.
///
/// # The defect this pins
///
/// The notes cap is a quarter of the route's byte budget, so it moves with the
/// route. `CarriedTurn::rebudget` used to restate `system_sources` and leave the
/// *block* as the assemble stage rendered it — so a 15,292-byte system prompt
/// built for the local tier met a floored route's 16,384-byte budget. On a first
/// attempt there is only one block to drop and it is the prompt the user just
/// typed, so `truncate_to_budget` reaches its second step instead: `room` for
/// that block's text collapses to its own 1,024-byte floor, and anything the
/// user wrote past that is middle-elided. The turn reaches the model carrying
/// the repository's description of itself and half the question about it.
///
/// **The message here is 2,000 bytes for that reason** — a pasted snippet, a
/// stack trace, a paragraph of context. Under ~1,024 the floor absorbs it and
/// the defect is invisible, which is what a 66-byte fixture would have proved
/// nothing about (measured on a synthetic message: 66 and 1,024 bytes survive
/// the unfixed reroute intact; 1,500 loses 476, 2,000 loses 976, 4,000 loses
/// 2,976 — the floor is the only thing standing between the user and the loss).
///
/// The fix re-renders the block at the new route's cap before the refit, which
/// is the party to the prompt a reroute *should* shrink. With it, a 4,000-byte
/// message survives whole.
///
/// # Why this is driven through `CarriedTurn`
///
/// The guard is the only value that lives for exactly one turn, and `rebudget`
/// is the only seam a reroute crosses. Both halves of the prompt — the base and
/// the file — are handed to `begin` the way the assemble stage hands them, and
/// the block itself is built by the **real** renderer from a real file
/// (LESSON-544: a hand-built `RepoContextBlock` would prove only that this
/// module can read a field it was handed).
///
/// # Mutation, run and observed
///
/// Deleting the `rerender_repo_context` call from `CarriedTurn::rebudget` fails
/// this test twice over, each half observed on its own by suppressing the other:
/// the block keeps the 8,192 bytes rendered for the route the turn left, and the
/// refit reports `newest_user_elided` with 883 bytes of this fixture's message
/// gone.
/// Passing `None` for the carry at `begin` fails it the same way, which is why
/// the parameter is a parameter and not a builder call.
///
/// # And the reroute announces what it re-rendered (verify, MAJOR 4)
///
/// The first fix re-rendered the block and told nobody. `repo_context_state` is
/// published by the assemble stage and by the two lifecycle sites; the reroute
/// was the fourth place the rendered figures move and the only one that was
/// silent, so a fallback onto a floored route cut the file with the marker
/// visible only inside a system prompt no client ever sees — BR-3's silence,
/// one seam over.
///
/// **This is why the test drives `refit_for_reroute` rather than
/// `CarriedTurn::rebudget`.** The publish is the *runtime's*, and a fixture that
/// called `rebudget` and then published its own event would be running a
/// protocol the daemon does not (LESSON-451, twice over in this very file). The
/// helper is `pub` for that reason.
///
/// Mutation, run and observed: deleting the `claim_repo_context_publish` block
/// from `refit_for_reroute` leaves the bus with the pressure line and no
/// `repo_context_state`; publishing unconditionally instead of through the
/// claim makes the second reroute — back to the route the turn started on —
/// announce a triple the client already has.
#[tokio::test]
async fn a_reroute_to_a_floored_route_re_renders_the_notes_and_keeps_the_users_message() {
    use std::sync::Arc as StdArc;
    use tetond::carry::RepoContextCarry;
    use tetond::harness::append_repo_context;
    use tetond::repo_context::{
        FileStat, RepoContextBlock, RepoContextFile, RepoContextState, REPO_CONTEXT_MAX_BYTES,
    };

    // 2,000 bytes: past `truncate_to_budget`'s 1,024-byte floor for the last
    // block's text, which is the threshold the defect hides below. See above.
    let message = format!(
        "Which crate owns the router, and what did REQ-590 change about it? \
         Here is the failing output I pasted in:\n{}",
        "  at crate::router::resolve (router.rs:1904)\n".repeat(40)
    );
    assert!(
        message.len() > 1_024 && message.len() < 4_096,
        "the fixture's message must be past the elision floor and inside the \
         room the fix leaves: {} bytes",
        message.len()
    );

    let sessions = SessionRegistry::new();
    let session = sessions
        .create(SessionMode::Freeform, None, None)
        .expect("a freeform session needs no phase")
        .session_id;
    let tools = ToolRegistry::with_builtins();
    let bus = Arc::new(EventBus::new());
    let mut bus_sub = bus.subscribe(64);
    let events = SessionEvents::new(Arc::clone(&bus), session.clone());

    // A `TETON.md` of exactly the ceiling, in whole 64-byte lines so the
    // line-boundary cut lands on the cap itself at both figures.
    let text = format!("{}\n", "n".repeat(63)).repeat(REPO_CONTEXT_MAX_BYTES / 64);
    assert_eq!(text.len(), REPO_CONTEXT_MAX_BYTES);
    let root = PathBuf::from("/repo");
    let path = root.join("TETON.md");
    let file = RepoContextFile {
        source: teton_protocol::methods::RepoContextSource::TetonMd,
        provenance: teton_core::ProvenanceId::from_resolved(&root, &path)
            .expect("`/repo/TETON.md` is a file under `/repo`"),
        bytes_on_disk: text.len() as u64,
        key: FileStat {
            len: text.len() as u64,
            mtime: None,
            is_symlink: false,
            is_regular: true,
            // A synthetic key: this file was never `stat`ed, so it has
            // no identity to carry and no second name to refuse.
            dev: 0,
            ino: 0,
            nlink: 1,
        },
        path,
        text,
    };
    let state = StdArc::new(RepoContextState::Loaded(file));

    // The local tier's route, and the prompt the assemble stage builds from it:
    // base with no block, then the block appended at the route's own cap.
    let local = HarnessConfig::default();
    assert_eq!(
        local.budget.repo_context_cap, REPO_CONTEXT_MAX_BYTES,
        "the fixture's first route must be one with the full cap, or the \
         re-render below has nothing to shrink"
    );
    let base_system = build_system_prompt(&tools, &local);
    assert!(
        !base_system.contains("<repo-notes"),
        "the base prompt is the one built with no block"
    );
    let block = RepoContextBlock::render(
        state.file().expect("the fixture's state carries a file"),
        local.budget.repo_context_cap,
    );
    assert_eq!(block.resident_bytes, REPO_CONTEXT_MAX_BYTES);
    // Cloned before the struct update moves `local`: the reroute below needs the
    // pair the turn was assembled against, which is what `refit_for_reroute`
    // compares the new one to.
    let local_budget = local.budget.clone();
    let config = HarnessConfig {
        repo_context: Some(block.clone()),
        ..local
    };
    let system = append_repo_context(&base_system, &block);

    let mut turn = CarriedTurn::begin(
        &sessions,
        &session,
        system,
        &config,
        Arc::new(SessionTaint::new()),
        Vec::new(),
        message.clone(),
        std::collections::BTreeSet::new(),
        false,
        Some(RepoContextCarry {
            base_system,
            state: StdArc::clone(&state),
        }),
    );
    assert!(
        turn.ctx().blocks().iter().any(|b| b.text == message),
        "the fixture seeded no user message, so the claim below is vacuous"
    );

    // The reroute: a provider whose declared window derives the floor, which is
    // where the quarter rule bites — 16,384 bytes of budget, 4,096 of notes.
    let floored = derive(BudgetInputs {
        window: 4_096,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("tiny"),
    });
    assert!(
        floored.floored,
        "the fixture's second route is not a floored one"
    );
    assert_eq!(floored.repo_context_cap, 4_096);
    // The daemon's own reroute seam — the function both of `run_prompt_turn`'s
    // reroute arms call — so the re-render, the claim, the publish and the
    // refit run in the order and under the gate production runs them.
    refit_for_reroute(&mut turn, &sessions, &events, &local_budget, &floored);

    // The block is the new route's, marker and all.
    let rendered = turn.ctx().system();
    let body = rendered
        .split_once("):\n")
        .expect("the block's naming sentence opens its body")
        .1;
    let kept = body
        .split_once("[… truncated")
        .expect("a block cut to the floored cap carries the marker")
        .0;
    assert_eq!(
        kept.len(),
        4_096,
        "the reroute kept the block rendered for the route it left"
    );
    assert!(
        rendered.contains("4,096-byte cap"),
        "the marker names the cap the block was actually rendered at"
    );
    // Everything above the notes is untouched: a reroute re-renders the block,
    // not the prompt.
    assert!(rendered.starts_with("You are Teton Code"));

    // The news: the file the session was carrying whole is now cut, and the
    // figures are the block's own — not the loader's ceiling-measured ones.
    //
    // Both event kinds come out of **one** drain: `try_recv` consumes the
    // queue, so a second helper over the same subscription would find it empty
    // and report an absence it created.
    let (announced, pressure) = drain_reroute(&mut bus_sub);
    assert_eq!(
        announced
            .iter()
            .map(|e| (e.state, e.truncated, e.resident_bytes))
            .collect::<Vec<_>>(),
        vec![(
            teton_protocol::methods::RepoContextStateKind::Truncated,
            true,
            4_096
        )],
        "a reroute cut the repository's notes in silence: {announced:?}"
    );
    assert_eq!(
        announced[0].bytes_on_disk,
        Some(REPO_CONTEXT_MAX_BYTES as u64)
    );

    // And the whole point: the user's message survived the refit, whole. Read
    // off the published line rather than a returned report, because the publish
    // is what a client actually sees.
    assert_eq!(pressure.len(), 1, "{pressure:#?}");
    assert!(
        !pressure[0].newest_user_elided,
        "the refit reported eliding the newest user block — {} bytes of the \
         user's own message were spent on the repository's notes",
        pressure[0].elided_bytes
    );
    assert!(
        turn.ctx().blocks().iter().any(|b| b.text == message),
        "the user's own message was elided to make room for the repository's \
         notes: {:?}",
        turn.ctx()
            .blocks()
            .iter()
            .map(|b| b.text.chars().take(60).collect::<String>())
            .collect::<Vec<_>>()
    );

    // Rerouting back to the route the turn started on puts the whole file back
    // and is news again; rerouting to the *same* budget twice is not. The claim
    // is what tells those apart, and it is the daemon's, not this fixture's.
    refit_for_reroute(&mut turn, &sessions, &events, &floored, &local_budget);
    let (back, _) = drain_reroute(&mut bus_sub);
    assert_eq!(
        back.iter()
            .map(|e| (e.state, e.truncated, e.resident_bytes))
            .collect::<Vec<_>>(),
        vec![(
            teton_protocol::methods::RepoContextStateKind::Loaded,
            false,
            REPO_CONTEXT_MAX_BYTES as u64
        )],
        "a file that stopped being truncated was not announced: {back:?}"
    );

    // A third route, whose pair is different again and whose **notes cap is the
    // same** — anything at or above 32,768 bytes of budget caps the notes at the
    // build's ceiling. The block is re-rendered and is byte-for-byte the one the
    // client already has, so the claim suppresses it. This is the leg that
    // distinguishes "publish what was rendered" from "publish through the gate":
    // without the claim it announces a line the client was already sent.
    let wide = derive(BudgetInputs {
        window: 128_000,
        cap: 0,
        reservation: 1_024,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    });
    assert_ne!(
        wide.budget_bytes, local_budget.budget_bytes,
        "the third route must move the pair, or `refit_for_reroute` returns \
         before it re-renders anything"
    );
    assert_eq!(
        wide.repo_context_cap, local_budget.repo_context_cap,
        "and must not move the notes cap, or the triple differs and the claim \
         is not what suppresses the line"
    );
    refit_for_reroute(&mut turn, &sessions, &events, &local_budget, &wide);
    let (same, same_pressure) = drain_reroute(&mut bus_sub);
    assert!(
        same.is_empty(),
        "a re-render that produced the line the client already has was \
         announced again: {same:?}"
    );
    assert_eq!(
        same_pressure.len(),
        1,
        "the pressure line is still owed — the budget moved: {same_pressure:?}"
    );

    // And a reroute that moves no budget at all re-renders nothing and says
    // nothing, on either channel.
    refit_for_reroute(&mut turn, &sessions, &events, &wide, &wide);
    let (quiet, quiet_pressure) = drain_reroute(&mut bus_sub);
    assert!(
        quiet.is_empty() && quiet_pressure.is_empty(),
        "a reroute that moved no budget announced something anyway: \
         {quiet:?} / {quiet_pressure:?}"
    );

    turn.abandon();
}

/// Every `repo_context_state` and every `context_pressure` queued on `events`
/// right now, oldest first, in **one** pass.
///
/// One pass because `try_recv` consumes: a second drain over the same
/// subscription finds an empty queue and would report an absence it created
/// itself. `try_recv` rather than a timed `recv` for [`drain_pressure`]'s
/// reason — the publishes are synchronous and already queued, and a wall-clock
/// window is the assertion shape that goes flaky first (LESSON-450).
fn drain_reroute(
    events: &mut tetond::broadcast::Subscription,
) -> (
    Vec<teton_protocol::events::RepoContextState>,
    Vec<ContextPressure>,
) {
    let (mut notes, mut pressure) = (Vec::new(), Vec::new());
    while let Some(envelope) = events.try_recv() {
        match envelope.event {
            Event::RepoContextState(state) => notes.push(state),
            Event::ContextPressure(cp) => pressure.push(cp),
            _ => {}
        }
    }
    assert!(
        !events.is_lagged(),
        "the subscription was evicted for falling behind, so an absent event \
         here would prove nothing"
    );
    (notes, pressure)
}
