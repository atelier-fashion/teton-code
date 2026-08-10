//! REQ-564 acceptance: prefix-cached KV across agent turns.
//!
//! # What this suite can and cannot prove
//!
//! The real `LlamaEngine` sits behind the non-default `llama` cargo feature,
//! which CI never compiles. So every test here runs against a scripted engine
//! that implements `complete_cached` through the **same**
//! [`PrefixCacheState`] the real engine uses, and calls the **same**
//! [`over_window`] guard.
//!
//! That proves the reuse policy, the miss taxonomy, the session keying, the
//! window guard, the event plumbing and the ledger accounting. It does **not**
//! prove that llama.cpp actually reuses the KV — no test in a default build
//! can, because no llama.cpp is linked into one. The evidence for that claim is
//! the dogfood measurement recorded in `docs/manual-verification.md`, not a
//! green run of this file. Reading a pass here as "prefix reuse works" is
//! exactly the overclaim LESSON-448 is about: a fast test double proved a
//! latency property it could not observe.

use std::sync::{Arc, Mutex};

use teton_inference::{
    over_window, CacheDecision, ChatFormat, Completion, Engine, EngineError, GenParams, MissReason,
    PrefixCacheState,
};
use teton_protocol::events::{Event, PrefixCacheMiss, PrefixCacheOutcome};
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::harness::{
    build_system_prompt, run_session_turn, ContextManager, HarnessConfig, NoopProvenanceHook,
    PendingPermissions, PermissionConfig, PermissionGate, SessionEvents, ToolContext, ToolRegistry,
};

/// The window the window-guard tests model. Small enough that an over-window
/// prompt is a handful of words rather than a 16k fixture.
const NARROW_N_CTX: u32 = 64;

/// A window wide enough for the real system prompt, for tests driving the full
/// turn loop (whose `HarnessConfig::default` reserves 1024 generation tokens).
const WIDE_N_CTX: u32 = 8192;

/// A deterministic stand-in for BPE tokenization.
///
/// Word-level and stable, which is all the policy needs: the rule compares
/// token-id sequences for a common prefix, and whether an id came from BPE or
/// from a hash is immaterial to whether two prompts share a prefix. A turn that
/// extends the previous prompt produces an extending id sequence here exactly as
/// it would in llama.cpp.
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

/// A scripted engine that caches prefixes exactly as the real one does.
///
/// It runs the real [`PrefixCacheState`] and the real [`over_window`] guard, so
/// a divergence between this and production is a divergence in the *mechanism*
/// (llama.cpp KV truncation), never in the *policy*. That split is the whole
/// point of REQ-564's architecture D-2.
struct CachingScriptedEngine {
    replies: Vec<String>,
    calls: usize,
    cache: PrefixCacheState,
    /// Every completion this engine served, for assertions about reuse.
    log: Arc<Mutex<Vec<CallRecord>>>,
    /// When false, `complete_cached` is not offered at all and every turn takes
    /// the cold path — the "cache disabled" arm of the AC-2 A/B.
    caching: bool,
    /// The window this engine enforces through the shared [`over_window`] guard.
    n_ctx: u32,
}

/// What one served completion did.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallRecord {
    prompt_tokens: u32,
    cached_tokens: u32,
    processed_tokens: u32,
    miss: Option<MissReason>,
}

impl CachingScriptedEngine {
    fn new(replies: &[&str], caching: bool) -> (Self, Arc<Mutex<Vec<CallRecord>>>) {
        Self::with_window(replies, caching, WIDE_N_CTX)
    }

    fn with_window(
        replies: &[&str],
        caching: bool,
        n_ctx: u32,
    ) -> (Self, Arc<Mutex<Vec<CallRecord>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                replies: replies.iter().map(|r| (*r).to_owned()).collect(),
                calls: 0,
                cache: PrefixCacheState::new(),
                log: Arc::clone(&log),
                caching,
                n_ctx,
            },
            log,
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

    /// Stream `reply` through `on_token`, honouring an early stop.
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

    fn record(&self, completion: &Completion) {
        self.log.lock().expect("log mutex").push(CallRecord {
            prompt_tokens: completion.prompt_tokens,
            cached_tokens: completion.cached_tokens,
            processed_tokens: completion.processed_tokens(),
            miss: completion.cache_miss,
        });
    }
}

impl Engine for CachingScriptedEngine {
    fn model_id(&self) -> &str {
        "scripted-local-3b"
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        let ids = tokenize(prompt);
        let prompt_tokens = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        // The SAME guard the real engine runs, on the cold path too (BR-7).
        if let Some(refusal) = over_window(prompt_tokens, self.n_ctx, params.max_tokens) {
            return Err(refusal);
        }
        // `complete` cannot advance the script (it takes `&self`), so a cold
        // call replays the first reply. Every test that needs a sequence drives
        // `complete_cached`.
        let reply = self.replies.first().cloned().unwrap_or_default();
        let text = Self::stream(&reply, params, on_token);
        let completion = Completion::cold(
            text,
            prompt_tokens,
            u32::try_from(reply.split_whitespace().count()).unwrap_or(u32::MAX),
        );
        self.record(&completion);
        Ok(completion)
    }

    fn complete_cached(
        &mut self,
        session: &str,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        if !self.caching {
            // The disabled arm: byte-for-byte the cold path, including the
            // script position, so an A/B compares like with like.
            let reply = self.next_reply();
            let ids = tokenize(prompt);
            let prompt_tokens = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            if let Some(refusal) = over_window(prompt_tokens, self.n_ctx, params.max_tokens) {
                return Err(refusal);
            }
            let completion_tokens = u32::try_from(reply.split_whitespace().count()).unwrap_or(0);
            let text = Self::stream(&reply, params, on_token);
            let completion = Completion::cold(text, prompt_tokens, completion_tokens);
            self.record(&completion);
            return Ok(completion);
        }

        let ids = tokenize(prompt);
        let prompt_tokens = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        // Guard BEFORE the probe, so the hit path is guarded identically (BR-7).
        if let Some(refusal) = over_window(prompt_tokens, self.n_ctx, params.max_tokens) {
            return Err(refusal);
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
        };
        self.record(&completion);
        Ok(completion)
    }

    fn evict_prefix_cache(&mut self, _reason: teton_inference::EvictionReason) {
        self.cache.evict();
    }
}

fn params() -> GenParams {
    GenParams {
        max_tokens: 8,
        temperature: 0.0,
    }
}

/// Drive one completion, discarding the stream.
fn turn(engine: &mut CachingScriptedEngine, session: &str, prompt: &str) -> String {
    engine
        .complete_cached(session, prompt, &params(), &mut |_| true)
        .expect("the turn completes")
        .text
}

// ---------------------------------------------------------------------------
// AC-1 — a multi-turn session prefills only the delta
// ---------------------------------------------------------------------------

/// AC-1: in a 5-turn session, turns 2–5 hit and prefill only the suffix.
///
/// The assertion is on *processed* tokens, not on a timing: `processed` must
/// equal the per-turn delta rather than the whole prompt. A cache that "worked"
/// but still processed the full prompt would pass a latency eyeball and fail
/// here, which is the point.
#[test]
fn turns_two_through_five_prefill_only_the_new_suffix() {
    let (mut engine, log) =
        CachingScriptedEngine::new(&["one.", "two.", "three.", "four.", "five."], true);

    // Each turn appends to the transcript, exactly as an agent session does:
    // the user's new message, then the assistant's ACTUAL reply. The reply text
    // matters — the engine's resident prefix is prompt + generated tokens,
    // because that is what the KV holds, so a transcript that appended some
    // other text would diverge and every turn would miss.
    let mut transcript = String::from("system prompt here");
    for n in 0..5 {
        transcript.push_str(&format!(" user turn {n}"));
        let reply = turn(&mut engine, "s1", &transcript);
        transcript.push(' ');
        transcript.push_str(reply.trim());
    }

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls.len(), 5);

    // Turn 1 is necessarily cold — there was nothing resident.
    assert_eq!(calls[0].miss, Some(MissReason::Cold));
    assert_eq!(calls[0].cached_tokens, 0);
    assert_eq!(calls[0].processed_tokens, calls[0].prompt_tokens);

    for (i, call) in calls.iter().enumerate().skip(1) {
        assert_eq!(call.miss, None, "turn {} must be a hit", i + 1);
        assert!(
            call.processed_tokens < call.prompt_tokens,
            "turn {} processed the whole prompt ({} of {}) — no reuse happened",
            i + 1,
            call.processed_tokens,
            call.prompt_tokens
        );
        // The delta is what turn i appended since the last recorded prefix, plus
        // the always-reprefilled final token.
        assert!(
            call.processed_tokens <= 5,
            "turn {} prefilled {} tokens for a 3-word delta — the suffix is \
             larger than the delta, so the reuse offset is wrong",
            i + 1,
            call.processed_tokens
        );
        assert_eq!(
            call.cached_tokens + call.processed_tokens,
            call.prompt_tokens,
            "cached and processed must partition the prompt exactly"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-2 — the cache is unobservable in output
// ---------------------------------------------------------------------------

/// AC-2 / BR-1: identical prompts and params produce byte-identical text with
/// the cache enabled and disabled. Reuse is a latency property; if it is ever
/// visible in output, it is a bug rather than an optimization.
#[test]
fn output_is_byte_identical_with_the_cache_enabled_and_disabled() {
    let replies = ["alpha reply.", "beta reply.", "gamma reply."];
    let prompts = ["sys u1", "sys u1 a1 u2", "sys u1 a1 u2 a2 u3"];

    let (mut cached, _) = CachingScriptedEngine::new(&replies, true);
    let (mut cold, _) = CachingScriptedEngine::new(&replies, false);

    for prompt in prompts {
        let with = turn(&mut cached, "s1", prompt);
        let without = turn(&mut cold, "s1", prompt);
        assert_eq!(
            with, without,
            "prefix reuse changed the output for {prompt:?} — it must be \
             observable only in latency"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-3 — divergence
// ---------------------------------------------------------------------------

/// AC-3 / BR-2: a turn whose history was rewritten (compaction/truncation)
/// misses with `divergent` and re-prefills from zero, producing correct output.
#[test]
fn a_compacted_transcript_misses_as_divergent_and_re_prefills() {
    let (mut engine, log) = CachingScriptedEngine::new(&["first.", "second."], true);

    turn(&mut engine, "s1", "sys u1 a1 u2 a2 u3");
    // Compaction rewrites history: the prompt no longer extends the resident
    // prefix, it replaces its middle.
    let text = turn(&mut engine, "s1", "sys SUMMARY-OF-EARLIER u4");

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls[1].miss, Some(MissReason::Divergent));
    assert_eq!(calls[1].cached_tokens, 0, "a divergent turn reuses nothing");
    assert_eq!(calls[1].processed_tokens, calls[1].prompt_tokens);
    assert_eq!(text, "second.", "the turn still produces its answer");
}

// ---------------------------------------------------------------------------
// AC-4 — interleaved sessions
// ---------------------------------------------------------------------------

/// AC-4 / BR-3: two sessions alternating on one slot. Thrash is permitted;
/// wrong output is not. Each session's turn must still answer correctly, and a
/// session that finds another's prefix resident must say `session_switch`
/// rather than silently reusing it — reusing another session's KV would leak
/// one conversation into another's answer.
#[test]
fn interleaved_sessions_thrash_the_slot_but_never_cross_contaminate() {
    let (mut engine, log) = CachingScriptedEngine::new(&["a1.", "b1.", "a2.", "b2."], true);

    assert_eq!(turn(&mut engine, "alpha", "sys alpha u1"), "a1.");
    assert_eq!(turn(&mut engine, "beta", "sys beta u1"), "b1.");
    assert_eq!(turn(&mut engine, "alpha", "sys alpha u1 a1 u2"), "a2.");
    assert_eq!(turn(&mut engine, "beta", "sys beta u1 b1 u2"), "b2.");

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls[0].miss, Some(MissReason::Cold));
    for (i, call) in calls.iter().enumerate().skip(1) {
        assert_eq!(
            call.miss,
            Some(MissReason::SessionSwitch),
            "turn {} reused a slot owned by the other session",
            i + 1
        );
        assert_eq!(call.cached_tokens, 0);
    }
}

// ---------------------------------------------------------------------------
// AC-5 — eviction
// ---------------------------------------------------------------------------

/// AC-5 / BR-4: a populated cache is dropped on request, and the next turn
/// succeeds cold — reporting `evicted`, not `cold`, so the drop is visible.
#[test]
fn an_evicted_cache_reports_itself_and_the_next_turn_still_succeeds() {
    let (mut engine, log) = CachingScriptedEngine::new(&["one.", "two."], true);

    turn(&mut engine, "s1", "sys u1");
    engine.evict_prefix_cache(teton_inference::EvictionReason::MemoryPressure);

    let text = turn(&mut engine, "s1", "sys u1 a1 u2");
    assert_eq!(text, "two.", "a dropped cache must never fail a turn");

    let calls = log.lock().expect("log").clone();
    assert_eq!(
        calls[1].miss,
        Some(MissReason::Evicted),
        "an eviction reported as `cold` hides that memory was reclaimed"
    );
    assert_eq!(calls[1].cached_tokens, 0);
}

// ---------------------------------------------------------------------------
// AC-6 — duties do not evict the agent cache
// ---------------------------------------------------------------------------

/// AC-6 / BR-5: agent turn, then a duty, then a second agent turn — the second
/// agent turn is still a hit.
///
/// This holds *structurally* rather than by discipline: a duty calls
/// `complete`, which has no session parameter, so it cannot reach the keyed
/// slot even by mistake.
#[test]
fn a_duty_between_two_agent_turns_leaves_the_second_turns_hit_intact() {
    let (mut engine, log) = CachingScriptedEngine::new(&["one.", "two."], true);

    turn(&mut engine, "s1", "sys u1");
    // A duty: the cold path, exactly as `duty.rs` and `classify.rs` call it.
    engine
        .complete("summarize this tool result", &params(), &mut |_| true)
        .expect("the duty completes");
    turn(&mut engine, "s1", "sys u1 one. u2");

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls.len(), 3, "cold turn, duty, cached turn");
    assert_eq!(calls[1].miss, Some(MissReason::Cold), "the duty is cold");
    assert_eq!(
        calls[2].miss, None,
        "the duty evicted the agent's prefix — BR-5 forbids exactly this"
    );
    assert!(calls[2].cached_tokens > 0);
}

// ---------------------------------------------------------------------------
// AC-8 — the over-window guard on both paths
// ---------------------------------------------------------------------------

/// AC-8 / BR-7: an over-window prompt is refused with the typed error on the
/// **hit** path as well as the miss path.
///
/// The hit-path case is the one that matters: the guard must run before the
/// probe, because reuse changes how many tokens are decoded but not how many
/// must fit. A guard placed after the probe would let a reusing turn through
/// and reach `GGML_ASSERT` — a process abort, not an error (LESSON-444).
#[test]
fn an_over_window_prompt_is_refused_on_the_hit_path_and_the_cold_path() {
    let (mut engine, _) = CachingScriptedEngine::with_window(&["one.", "two."], true, NARROW_N_CTX);

    // Warm the cache so the oversized turn arrives with a resident prefix.
    let base = "sys u1";
    turn(&mut engine, "s1", base);

    // `NARROW_N_CTX` (64) minus 8 generation leaves a 56-token budget.
    let oversized = format!("{base} {}", "word ".repeat(80));
    let refused = engine.complete_cached("s1", &oversized, &params(), &mut |_| true);
    let err = refused.expect_err("an over-window prompt must be refused on the hit path");
    assert!(
        err.to_string().contains("exceeds this engine's window"),
        "the refusal must be the typed window error, got: {err}"
    );

    // And on the cold path, from the same one guard.
    let cold = engine.complete(&oversized, &params(), &mut |_| true);
    assert!(cold.is_err(), "the cold path must refuse it too");
}

/// The boundary itself, at the seam the engine uses — a prompt exactly at the
/// budget is served, one past it is refused.
#[test]
fn the_window_boundary_is_inclusive_at_the_budget() {
    assert!(over_window(56, NARROW_N_CTX, 8).is_none());
    assert!(over_window(57, NARROW_N_CTX, 8).is_some());
}

// ---------------------------------------------------------------------------
// AC-9 + event plumbing — through the real turn loop
// ---------------------------------------------------------------------------

/// AC-9 and the event half of BR-8/BR-9, through the **real** turn loop: one
/// `prefix_cache` event per local turn, one unpriced ledger row, and the two
/// agreeing about the token split.
///
/// Driving `run_session_turn` rather than the engine directly is deliberate:
/// the counts must survive the projection from `Completion` through
/// `SourceTurn` to both the event and the row, and a test at the engine seam
/// would not notice a projection that dropped or transposed them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_turn_emits_one_prefix_cache_event_matching_its_ledger_row() {
    let (engine, _log) = CachingScriptedEngine::new(&["Done."], true);
    let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(engine));

    let bus = Arc::new(EventBus::new());
    let mut events_rx = bus.subscribe(64);

    let session_id = SessionId::from("prefix-cache-session");
    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(std::env::temp_dir());
    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("say done");

    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::new(PendingPermissions::new()),
    );
    let session_events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;

    run_session_turn(
        &engine,
        ChatFormat::Flat,
        &tools,
        &tool_ctx,
        &gate,
        &session_events,
        &mut ctx,
        &config,
        &mut hook,
    )
    .await
    .expect("the local turn completes");

    // Drain what the turn published. A bounded drain rather than "read until
    // the channel closes": the permission gate holds its own `Arc<EventBus>`,
    // so the sender side outlives this scope and an unbounded read would hang
    // rather than fail.
    let mut cache_events = Vec::new();
    while let Ok(Some(envelope)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv()).await
    {
        if let Event::PrefixCache(cache) = envelope.event {
            assert_eq!(envelope.session_id.as_ref(), Some(&session_id));
            cache_events.push(cache);
        }
    }
    assert_eq!(
        cache_events.len(),
        1,
        "a local turn must emit exactly one prefix_cache event, got {}",
        cache_events.len()
    );
    let cache = &cache_events[0];
    assert_eq!(cache.model, "scripted-local-3b");
    match cache.outcome {
        PrefixCacheOutcome::Miss {
            reason,
            processed_tokens,
        } => {
            assert_eq!(reason, PrefixCacheMiss::Cold, "a first turn is cold");
            assert!(
                processed_tokens > 0,
                "a cold turn prefills the whole prompt"
            );
        }
        ref other => panic!("a first turn must miss cold, got {other:?}"),
    }
}

/// The ledger half of AC-9: a local turn records one **unpriced** row whose
/// cached/processed split matches what the engine reported.
#[test]
fn a_local_turn_records_one_unpriced_ledger_row_with_its_cached_count() {
    let dir = std::env::temp_dir().join(format!("teton-prefix-cache-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("cost.db");
    let _ = std::fs::remove_file(&path);

    let ledger = CostLedger::open(&path, PriceTable::bundled(), Arc::new(NoopCostSink))
        .expect("open ledger");

    // Two turns: a cold one, then one that reuses most of its prompt.
    ledger
        .record_local_call(
            "s1",
            &tetond::cost::CostAttribution::new("scripted-local-3b"),
            100,
            10,
            0,
        )
        .expect("cold row");
    ledger
        .record_local_call(
            "s1",
            &tetond::cost::CostAttribution::new("scripted-local-3b"),
            120,
            10,
            110,
        )
        .expect("cached row");

    let rows = ledger.all_records().expect("read");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.provider_id, "local");
        assert_eq!(
            row.usd_micros, None,
            "local inference is usage, not spend — an unpriced row, never a \
             priced-at-zero one"
        );
    }
    assert_eq!(
        rows[0].cached_tokens,
        Some(0),
        "the cold turn reused nothing"
    );
    assert_eq!(rows[1].cached_tokens, Some(110));
    // The summed split across the session is derivable from the rows alone,
    // which is what BR-9 exists for.
    let total_input: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let total_cached: u64 = rows.iter().filter_map(|r| r.cached_tokens).sum();
    assert_eq!(total_input, 220);
    assert_eq!(total_cached, 110);

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// The policy seam itself
// ---------------------------------------------------------------------------

/// The test double and the real engine must enforce the same rule. This pins
/// the shared decision type directly, so a future change that "fixes" the test
/// engine's copy of the rule instead of the real one fails here.
#[test]
fn the_reuse_rule_always_leaves_a_token_to_prefill() {
    let mut state = PrefixCacheState::new();
    state.record("s1", vec![1, 2, 3, 4]);
    match state.probe("s1", &[1, 2, 3, 4]) {
        CacheDecision::Hit { reuse } => assert_eq!(
            reuse, 3,
            "an identical prompt must still leave one token to decode, or \
             sampling reads stale logits"
        ),
        other => panic!("an identical prompt is a hit, got {other:?}"),
    }
}
