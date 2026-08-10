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
//! rather than to re-implement. It seeds each prompt through the same three
//! seams the daemon's dispatch uses — [`SessionRegistry::conversation_snapshot`],
//! [`ContextManager::replay_blocks`], [`SessionRegistry::commit_conversation`]
//! — because the runtime's engine slot and config are private to the crate and
//! an integration test cannot install a recording engine into a
//! `DaemonRuntime`. That is why AC-1/AC-11/AC-12 live in the in-crate module:
//! their claim is about the **dispatch**, and only an in-crate test can watch
//! it. The claim here is about what the cache and the ledger do with a carried
//! conversation once dispatch has handed one over.
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
//! **Mutation 1 — the dispatch stops seeding.** `ctx.replay_blocks(sessions
//! .conversation_snapshot(&session_id))` commented out of
//! `DaemonRuntime::run_prompt_turn`, leaving literally the pre-REQ dispatch
//! (`ContextManager::new`, then `push_user`). Five in-crate tests and both e2e
//! legs went red:
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
//! The tests in *this* file stayed green under it, and that is the honest
//! reading of where they sit: they seed through the registry seams themselves,
//! so a dispatch that stopped calling those seams is invisible to them.
//!
//! **Mutation 2 — the store stops recording.** `SessionRegistry::
//! commit_conversation` returned before its write. The same end state by the
//! other route — every prompt replays an empty snapshot, so every prompt is a
//! fresh context — and it reddens this file too: nine in-crate tests, both e2e
//! legs, and, new here:
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
//! run because AC-2 deserves it: `ContextManager::replay_blocks` replayed tool
//! blocks through `push_model`, keeping every byte and dropping the
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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use teton_inference::{
    over_window, ChatFormat, Completion, Engine, EngineError, GenParams, MissReason,
    PrefixCacheState,
};
use teton_protocol::events::{Event, PrefixCacheOutcome};
use teton_protocol::{SessionId, SessionMode};

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, LocalUsageMeter, NoopCostSink, PriceTable};
use tetond::harness::compact::COMPACT_OUTPUT_CONTRACT;
use tetond::harness::context::{approx_tokens, APPROX_BYTES_PER_TOKEN};
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, DutyRoute, HarnessConfig,
    HarnessError, LocalEngineSource, NoopProvenanceHook, PendingPermissions, PermissionConfig,
    PermissionGate, SessionEvents, ToolContext, ToolDuties, ToolRegistry, TurnOutcome,
    COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TRIAGE_DUTY,
};
use tetond::sessions::SessionRegistry;

/// A window wide enough for the real system prompt plus several carried turns.
/// The point of these tests is the reuse arithmetic, not the window guard —
/// which `prefix_cache_session.rs` pins on both paths.
const WIDE_N_CTX: u32 = 32_768;

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
            // A tool result enters context whole: these fixtures fill the budget
            // deliberately, and a digest duty condensing them first would be the
            // thing under test rather than compaction.
            summarize_threshold_tokens: usize::MAX,
            ..HarnessConfig::default()
        };
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

    /// One prompt turn, seeded and committed exactly as
    /// `DaemonRuntime::run_prompt_turn` does it: rebuild the head, replay the
    /// session's committed blocks under it, push the new message, run the loop,
    /// and commit the manager's post-turn blocks **only** on success (D-1).
    async fn prompt(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<TurnOutcome, HarnessError> {
        let gate = PermissionGate::new(
            session_id.clone(),
            PermissionConfig::permissive(),
            Arc::clone(&self.bus),
            Arc::new(PendingPermissions::new()),
        );
        let events = SessionEvents::new(Arc::clone(&self.bus), session_id.clone());

        let system = build_system_prompt(&self.tools, &self.config);
        let mut ctx = ContextManager::new(system, self.config.context_budget_tokens)
            .with_budget_bytes(self.config.context_budget_bytes);
        ctx.replay_blocks(self.sessions.conversation_snapshot(session_id));
        ctx.push_user(text);

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
            &mut ctx,
            &self.config,
            &mut hook,
            &digest,
            &compact,
            &ToolDuties {
                triage: &triage,
                shell: &shell,
            },
        )
        .await;

        if outcome.is_ok() {
            self.sessions
                .commit_conversation(session_id, ctx.into_blocks());
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
            .iter()
            .map(|block| approx_tokens(&block.text))
            .sum()
    }

    fn retained_blocks(&self, session_id: &SessionId) -> usize {
        self.sessions.conversation_snapshot(session_id).len()
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
                .into_iter()
                .map(|block| (block.role, block.text))
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
/// regression: the manager that ends a truncating turn has rendered its prompts
/// with the "[earlier conversation truncated]" note, and the next prompt builds
/// a fresh manager from the committed blocks — which has not truncated
/// anything yet, and so does not carry the note. The rendered prompts therefore
/// disagree immediately after the system head, and REQ-564's amended BR-2 reuses
/// exactly up to the disagreement. That is the "compacted boundary may
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
        assert_eq!(
            boundary.cached_tokens,
            head_tokens,
            "boundary {} reused {} tokens; a rewritten history means the common \
             prefix is the system head and nothing past it",
            n + 1,
            boundary.cached_tokens
        );
        assert_eq!(
            boundary.cached_tokens + boundary.processed_tokens,
            boundary.prompt_tokens,
            "the rewritten tail must be re-prefilled in full"
        );
    }
}
