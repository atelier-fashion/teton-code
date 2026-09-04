//! Remote execution through the turn loop (the TASK-010 integration gap, closed).
//!
//! TASK-009/010 landed the loop local-first: it drove the local `Engine` and
//! nothing else, so a phase routed to a remote model had nowhere to actually run.
//! Part A of TASK-013 introduced the [`CompletionSource`] abstraction so the *same*
//! loop drives either the local engine or a remote `Provider`. These tests prove
//! the remote path end to end — through the **real** OpenAI-compatible adapter and
//! the **real** egress choke point + cost ledger the daemon uses — and assert that
//! a remote-routed session:
//!
//! 1. streams tokens (multiple `agent_message_chunk`s within a single turn),
//! 2. dispatches tools (a real read → edit → verify → done flow that edits a file
//!    on disk),
//! 3. records cost (one attributed `CostRecord` per remote turn, BR-2), and
//! 4. honors privacy boundaries (a turn whose context touched a `local-only` file
//!    is blocked before any byte leaves, emits `privacy_block`, and bills nothing —
//!    BR-1).
//!
//! The offline, transport-free local path is unchanged and still proven by
//! `tests/offline_session.rs` (AC-1).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::ProvenanceId;

use async_trait::async_trait;

use teton_protocol::events::{Event, SessionUpdatePayload};
use teton_protocol::methods::StopReason;
use teton_protocol::{Phase, SessionId};
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::Egress;

/// Mint the identity of a fixture file (REQ-571 ADR-A).
///
/// The provenance channel accepts only a [`ProvenanceId`], and an integration
/// test cannot reach the crate-internal fixture helper, so each test binary
/// states its own. A fixture naming a path that is not an identity is a broken
/// fixture, hence the panic.
fn source_id(path: &str) -> ProvenanceId {
    ProvenanceId::claimed(path).expect("fixture path must be a provenance id")
}

use tetond::harness::budget::{derive, BudgetInputs};
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, DutyRoute, HarnessConfig,
    HarnessError, NoopProvenanceHook, PendingPermissions, PermissionConfig, PermissionGate,
    RemoteProviderSource, SessionEvents, ToolContext, ToolDuties, ToolRegistry,
};

// --------------------------------------------------------------------------
// A scripted OpenAI-compatible transport: one canned SSE body per call.
// --------------------------------------------------------------------------

/// A `Transport` that returns a queue of pre-scripted OpenAI-compatible SSE
/// bodies (one per remote turn) and records the request bodies it was asked to
/// send (so a test can inspect what actually reached the wire).
#[derive(Clone, Default)]
struct ScriptedSseTransport {
    bodies: Arc<Mutex<VecDeque<String>>>,
    calls: Arc<AtomicUsize>,
    /// Every request body handed to `execute`, parsed — the bytes the real
    /// adapter built, which is the only place a wire-shape claim can be
    /// discharged (conventions.md: code inspection is not acceptance).
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ScriptedSseTransport {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The request bodies sent so far, in order.
    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for ScriptedSseTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).expect("the adapter sends a JSON body"));
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(futures::stream::once(async move { Ok(body.into_bytes()) })),
        })
    }
}

/// One OpenAI-compatible streaming turn: `content` deltas, then an optional tool
/// call, then usage + `[DONE]`. Splitting `content` into several deltas is what
/// lets the token-streaming assertion see more than one chunk per turn.
fn sse_turn(
    content_deltas: &[&str],
    tool: Option<(&str, &str, &str)>, // (id, name, arguments-json)
    prompt_tokens: u64,
    completion_tokens: u64,
) -> String {
    let mut s = String::new();
    for delta in content_deltas {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "content": delta } }]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
    }
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": { "name": name, "arguments": args }
                }]}
            }]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    let usage = serde_json::json!({
        "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens }
    });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

fn temp_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-remote-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub const ANSWER: u32 = 1;\n").unwrap();
    root
}

fn ledger() -> Arc<CostLedger> {
    Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger"),
    )
}

/// Drain every event currently buffered on `sub`.
async fn collect_events(
    sub: &mut tetond::broadcast::Subscription,
) -> Vec<teton_protocol::events::EventEnvelope> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        out.push(env);
    }
    out
}

// --------------------------------------------------------------------------
// 1. A remote session streams tokens, dispatches tools, and records cost.
// --------------------------------------------------------------------------

#[tokio::test]
async fn remote_routed_session_streams_dispatches_tools_and_records_cost() {
    let repo = temp_repo();

    // The remote model's scripted plan: read → edit → verify → done — the same
    // shape the offline test drives locally, but every turn now streams from a
    // real provider adapter through egress. Turn 1's text is split into two deltas
    // so the streaming assertion sees intra-turn token flow.
    let bodies = vec![
        sse_turn(
            &["Reading ", "the file."],
            Some(("call_1", "read", r#"{"path":"src/lib.rs"}"#)),
            120,
            20,
        ),
        sse_turn(
            &["Editing the constant."],
            Some((
                "call_2",
                "edit",
                r#"{"path":"src/lib.rs","old_string":"pub const ANSWER: u32 = 1;","new_string":"pub const ANSWER: u32 = 2;"}"#,
            )),
            160,
            40,
        ),
        sse_turn(
            &["Verifying the change."],
            Some((
                "call_3",
                "shell",
                r#"{"command":"grep -q 'ANSWER: u32 = 2' src/lib.rs && echo VERIFIED"}"#,
            )),
            190,
            30,
        ),
        sse_turn(&["Done. ANSWER is now 2 and verified."], None, 210, 15),
    ];

    let transport = ScriptedSseTransport::with_bodies(bodies);
    let cost = ledger();
    // The REAL egress choke point + cost ledger — no boundaries here.
    let egress = Egress::new(transport, Vec::new(), Arc::new(tetond::egress::NoopSink))
        .with_cost_meter(cost.clone());

    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));

    let session_id = SessionId::from("remote-1");
    // Implement-phase attribution: a cheap remote model executing the implement
    // turn (AC-3 shape), billed per phase (BR-2).
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        // REQ-559: an integration fixture states its effort like any other
        // call path — the field is required, so it cannot be forgotten.
        ResolvedEffort::effort(EffortLevel::High),
    )
    .with_phase(Phase::Implement);

    let config = HarnessConfig::default(); // weak-model shape: verification required
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("In src/lib.rs change ANSWER from 1 to 2, then verify it.");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    // No local tier on this machine: summarizer is None (remote-only operation).
    let outcome = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        // REQ-558: this loop digests through the `digest` category. These turns
        // stay under the summarization threshold, so nothing is served — and an
        // unresolved route bounds mechanically rather than folding raw.
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        // REQ-561: and no tool duty either. `triage` would rank a `grep`
        // result; these turns run no multi-match `grep`, and an unresolved
        // route returns the tool's own result unchanged.
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await
    .expect("remote turn completes");

    // (2) Tools dispatched: the turn ended cleanly having edited AND verified.
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(outcome.edited, "the remote model's edit landed");
    assert!(outcome.verified, "the edit was verified");
    let updated = std::fs::read_to_string(repo.join("src/lib.rs")).unwrap();
    assert!(
        updated.contains("pub const ANSWER: u32 = 2;"),
        "file was not edited by the remote-routed session: {updated}"
    );

    // (3) Cost recorded: one attributed CostRecord per remote turn (BR-2).
    let rows = cost.all_records().expect("read ledger");
    assert_eq!(rows.len(), 4, "one CostRecord per remote turn");
    for row in &rows {
        assert_eq!(row.session_id, "remote-1");
        assert_eq!(row.provider_id, "deepseek");
        assert_eq!(row.model, "deepseek-chat");
        assert_eq!(row.phase, Some(Phase::Implement), "per-phase attribution");
    }
    // Token counts came from the streamed usage of each turn.
    assert_eq!(rows[0].input_tokens, 120);
    assert_eq!(rows[1].output_tokens, 40);

    // (1) Tokens streamed: the assistant text arrived as multiple chunks, and the
    // first turn produced more than one chunk on its own (intra-turn streaming).
    let evs = collect_events(&mut sub).await;
    let chunks: Vec<String> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::SessionUpdate(su) => match &su.update {
                SessionUpdatePayload::AgentMessageChunk { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        chunks.len() >= 5,
        "expected streamed token chunks across turns, got {}: {chunks:?}",
        chunks.len()
    );
    let streamed = chunks.join("");
    assert!(
        streamed.contains("Reading the file."),
        "streamed: {streamed}"
    );
    assert!(streamed.contains("Editing the constant."));

    std::fs::remove_dir_all(&repo).ok();
}

// --------------------------------------------------------------------------
// 2. A remote turn whose context touched a local-only file is blocked (BR-1).
// --------------------------------------------------------------------------

#[tokio::test]
async fn remote_turn_over_boundary_context_is_blocked_and_never_billed() {
    let repo = temp_repo();

    // The transport is scripted with a body, but a boundary block must prevent it
    // from ever being reached: the assertion is that zero bytes leave.
    let transport =
        ScriptedSseTransport::with_bodies(vec![sse_turn(&["should never send"], None, 1, 1)]);
    let calls = Arc::clone(&transport.calls);
    let cost = ledger();

    let bus = Arc::new(EventBus::new());
    // Egress with a `secrets/**` local-only boundary; privacy_block events flow to
    // the bus, cost to the ledger.
    let boundaries = vec![teton_core::entities::PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: teton_core::entities::BoundaryMode::LocalOnly,
        origin: Default::default(),
    }];
    let egress = Egress::new(transport, boundaries, bus.clone()).with_cost_meter(cost.clone());

    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));

    let session_id = SessionId::from("remote-boundary");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        // REQ-559: an integration fixture states its effort like any other
        // call path — the field is required, so it cannot be forgotten.
        ResolvedEffort::effort(EffortLevel::High),
    )
    .with_phase(Phase::Implement);

    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("Summarize the production config.");
    // The session already read a local-only file: its content is in context,
    // tagged with the boundary path. Any remote turn from here must be blocked.
    ctx.push_tool_result(
        "read",
        Some(source_id("secrets/prod.env")),
        "API_KEY=sk-live-DO-NOT-LEAK-abc123",
    );

    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        // REQ-558: this loop digests through the `digest` category. These turns
        // stay under the summarization threshold, so nothing is served — and an
        // unresolved route bounds mechanically rather than folding raw.
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        // REQ-561: and no tool duty either. `triage` would rank a `grep`
        // result; these turns run no multi-match `grep`, and an unresolved
        // route returns the tool's own result unchanged.
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    // The loop surfaced the block as a remote error (the turn could not run).
    match result {
        Err(HarnessError::Remote(_)) => {}
        other => panic!("expected a remote/boundary error, got {other:?}"),
    }

    // Not a single byte left the machine: the inner transport was never called.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "boundary content must be blocked before any network call"
    );
    // Nothing was billed — a blocked call is never a CostRecord (BR-1/BR-2).
    assert!(
        cost.all_records().expect("read ledger").is_empty(),
        "a blocked remote turn must produce no CostRecord"
    );

    // A privacy_block event was emitted, naming the offending path and provider.
    let evs = collect_events(&mut sub).await;
    let blocks: Vec<_> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::PrivacyBlock(pb) => Some(pb),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 1, "exactly one privacy_block (BR-1)");
    assert_eq!(blocks[0].path, "secrets/prod.env");
    assert_eq!(
        blocks[0].provider_id,
        teton_protocol::ProviderId::from("deepseek")
    );

    std::fs::remove_dir_all(&repo).ok();
}

// --------------------------------------------------------------------------
// 3. BUG-178: a native tool call with no prose never replays an empty
//    assistant turn.
// --------------------------------------------------------------------------

/// One OpenAI-compatible streaming turn in the **kimi-k3 shape** (BUG-178): the
/// model streams `reasoning_content`, then a `tool_calls` fragment, and every
/// `content` delta is the empty string — there is no prose at all. Moonshot
/// answers the harness's *next* request with HTTP 400 when that turn is
/// replayed as `{"role":"assistant","content":""}`.
fn kimi_reasoning_only_tool_call_turn(id: &str, name: &str, args: &str) -> String {
    let mut s = String::new();
    let role = serde_json::json!({
        "choices": [{ "delta": { "role": "assistant", "content": "" } }]
    });
    s.push_str(&format!("data: {role}\n\n"));
    for thought in ["I should look at ", "the file first."] {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "content": "", "reasoning_content": thought } }]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
    }
    let call = serde_json::json!({
        "choices": [{
            "delta": { "content": "", "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            }]}
        }]
    });
    s.push_str(&format!("data: {call}\n\n"));
    let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
    s.push_str(&format!("data: {finish}\n\n"));
    let usage = serde_json::json!({
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 30,
            "completion_tokens_details": { "reasoning_tokens": 20 }
        }
    });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

#[tokio::test]
async fn a_native_tool_call_with_no_prose_never_replays_an_empty_assistant_turn() {
    let repo = temp_repo();

    // Turn 1: kimi-k3 answers with reasoning + a native `read` call and no
    // prose. Turn 2: having seen the file, it answers in prose. Before the fix,
    // turn 2's request carried an empty assistant message for turn 1 and
    // Moonshot refused it — every kimi tool-using turn died there.
    let bodies = vec![
        kimi_reasoning_only_tool_call_turn("call_1", "read", r#"{"path":"src/lib.rs"}"#),
        sse_turn(&["src/lib.rs defines ANSWER = 1."], None, 180, 12),
    ];
    let transport = ScriptedSseTransport::with_bodies(bodies);
    let sent = transport.clone();
    let cost = ledger();
    let egress = Egress::new(transport, Vec::new(), Arc::new(tetond::egress::NoopSink))
        .with_cost_meter(cost.clone());

    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "kimi",
        "https://api.moonshot.ai/v1/chat/completions",
    ));

    let session_id = SessionId::from("remote-kimi");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "kimi",
        "kimi-k3",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    )
    .with_phase(Phase::Implement);

    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("What does src/lib.rs define?");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let outcome = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await
    .expect("the turn completes: the read runs and the model answers");

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(outcome.final_text, "src/lib.rs defines ANSWER = 1.");

    // The wire: two requests left through the real adapter. The SECOND is the
    // one that used to fail — it replays turn 1.
    let requests = sent.requests();
    assert_eq!(requests.len(), 2, "one request per remote turn");
    let follow_up = requests[1]["messages"]
        .as_array()
        .expect("chat/completions messages array");

    // (a) No assistant message anywhere in it is empty — the shape Moonshot
    //     rejects with `must not be empty` never leaves the machine.
    for m in follow_up {
        if m["role"] == "assistant" {
            let content = m["content"].as_str().unwrap_or_default();
            assert!(
                !content.trim().is_empty(),
                "an empty assistant message reached the wire: {follow_up:#?}"
            );
        }
    }
    // (b) Turn 1 is recorded as the call the model made — the transcript says
    //     what happened, in the tool-call shape the system prompt teaches.
    let assistant_turns: Vec<&str> = follow_up
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(assistant_turns.len(), 1, "{follow_up:#?}");
    assert_eq!(
        assistant_turns[0],
        r#"{"tool":"read","arguments":{"path":"src/lib.rs"}}"#
    );
    // (c) …and the tool result folds in as the user turn after it, so the
    //     provider sees call → result → (its answer), strictly alternating.
    let roles: Vec<&str> = follow_up
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "user"],
        "{follow_up:#?}"
    );
    let folded = follow_up[3]["content"].as_str().unwrap();
    assert!(folded.contains("Tool result (read):"), "{folded}");
    assert!(folded.contains("pub const ANSWER: u32 = 1;"), "{folded}");

    // The stand-in is transcript, not display: the user never saw raw JSON.
    let evs = collect_events(&mut sub).await;
    let streamed: String = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::SessionUpdate(su) => match &su.update {
                SessionUpdatePayload::AgentMessageChunk { text } => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        !streamed.contains("\"tool\""),
        "the recorded call must not stream to the user: {streamed}"
    );
    assert!(streamed.contains("src/lib.rs defines ANSWER = 1."));

    // Both turns billed, with the reasoning split the kimi shape reports.
    let rows = cost.all_records().expect("read ledger");
    assert_eq!(rows.len(), 2, "one CostRecord per remote turn");
    assert_eq!(rows[0].reasoning_tokens, Some(20));

    std::fs::remove_dir_all(&repo).ok();
}

// --------------------------------------------------------------------------
// REQ-586 AC-2 / AC-3: the budget is the route's, and an overflow is typed
// --------------------------------------------------------------------------

/// A transport that answers **every** request with a 400 naming the context
/// window, and counts them.
///
/// The counting is the point: BR-2 says a context-length refusal does not
/// retry, and a transport that answered a second request with something usable
/// would let a swallowed refusal look like a completed turn.
#[derive(Clone)]
struct RefuseOversized {
    calls: Arc<AtomicUsize>,
    body: &'static str,
}

impl RefuseOversized {
    fn new(body: &'static str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            body,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for RefuseOversized {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = self.body;
        Ok(TransportResponse {
            location: None,
            status: 400,
            body: Box::pin(futures::stream::once(async move {
                Ok(body.as_bytes().to_vec())
            })),
        })
    }
}

/// Moonshot/Kimi's documented overflow body (`platform.kimi.ai/docs/api/errors`,
/// verified 2026-08-19) — the dogfood provider's own spelling, driven here
/// through the whole daemon-side path rather than only through the adapter's
/// unit tests.
const KIMI_TOO_LONG: &str =
    r#"{"error":{"type":"invalid_request_error","message":"Input token length too long"}}"#;

/// A prompt of `words` distinct whitespace-separated words at **4 bytes each**
/// (three base-36 characters and a separator, so exactly `4 × words − 1`).
///
/// Two properties the AC needs and a lorem-ipsum string would not give it.
///
/// The density is **fixed and known**, which is what lets a caller state which
/// of the two guards its fixture is standing on. It is deliberately *not* below
/// the remote pair's implied ratio: a remote route budgets `usable × 2 / 3`
/// words against `usable × 2` bytes, i.e. ≈ 3 B per word of budget, so a prompt
/// at 4 B/word sized to fill the *word* budget would overflow the byte one by a
/// third. Nothing here is sized that way — every caller sits far under both —
/// and each one asserts the byte fit against the route's own
/// `context_budget_bytes` rather than inferring it from this density (REQ-586
/// AC F-19: a fixture-honesty claim is one a reader can check).
///
/// And every word is distinct, so "the body contains all of it" is a real
/// claim: a middle elision removes words that are nowhere else in the string,
/// where a repeated filler would still look whole.
fn distinct_words(words: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    assert!(
        words <= ALPHABET.len().pow(3),
        "the three-character alphabet cannot mint {words} distinct words"
    );
    let mut out = String::with_capacity(words * 4);
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        out.push(ALPHABET[(i / 1_296) % 36] as char);
        out.push(ALPHABET[(i / 36) % 36] as char);
        out.push(ALPHABET[i % 36] as char);
    }
    out
}

/// The `HarnessConfig` a route to a provider with `window` tokens runs under —
/// built through the **one** derivation the router uses, never by setting the
/// pair by hand (BR-8, AC-12).
fn route_config(window: u32) -> HarnessConfig {
    HarnessConfig::for_strong_model().with_route_budget(derive(BudgetInputs {
        window,
        cap: 0,
        // ADR-1's reservation: the `max_tokens` the adapters actually send.
        reservation: HarnessConfig::default().gen_params.max_tokens,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
        local_window: 0,
    }))
}

/// Seed a manager against `config`'s whole budget — pair and window label —
/// exactly as [`CarriedTurn::begin`] does for a real turn.
fn seeded(config: &HarnessConfig, system: String, prompt: &str) -> ContextManager {
    let mut ctx = ContextManager::new(system, config.context_budget_tokens)
        .with_budget_bytes(config.context_budget_bytes)
        .with_window_label(config.budget.window_label.clone());
    ctx.push_user(prompt);
    ctx
}

/// Everything one remote turn needs besides its source.
struct LoopRig {
    tools: ToolRegistry,
    tool_ctx: ToolContext,
    gate: PermissionGate,
    events: SessionEvents,
    bus: Arc<EventBus>,
}

fn loop_rig(session_id: &SessionId) -> LoopRig {
    let bus = Arc::new(EventBus::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::new(PendingPermissions::new()),
    );
    LoopRig {
        tools: ToolRegistry::with_builtins(),
        tool_ctx: ToolContext::new(std::env::temp_dir()),
        gate,
        events: SessionEvents::new(Arc::clone(&bus), session_id.clone()),
        bus,
    }
}

async fn drive(
    source: &mut dyn tetond::harness::CompletionSource,
    rig: &LoopRig,
    ctx: &mut ContextManager,
    config: &HarnessConfig,
) -> Result<tetond::harness::TurnOutcome, HarnessError> {
    let mut hook = NoopProvenanceHook;
    run_session_turn_with_source(
        source,
        &rig.tools,
        &rig.tool_ctx,
        &rig.gate,
        &rig.events,
        ctx,
        config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await
}

/// Every `context_pressure` in `events`.
fn pressure_events(
    events: &[teton_protocol::events::EventEnvelope],
) -> Vec<teton_protocol::events::ContextPressure> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ContextPressure(cp) => Some(*cp),
            _ => None,
        })
        .collect()
}

/// Everything the turn streamed into its own output, concatenated — the path
/// BR-7's newest-block notice shares with the model's prose, which is the
/// point of putting it there rather than only on the event stream.
fn streamed_text(events: &[teton_protocol::events::EventEnvelope]) -> String {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::SessionUpdate(su) => match &su.update {
                SessionUpdatePayload::AgentMessageChunk { text } => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// **AC-2, both halves.** The same 20,000-word prompt against two budgets: on
/// a 128k route it is assembled **whole**, in one request, with nothing dropped
/// and nothing elided; on the default (local) pair it is clamped, and the clamp
/// is announced.
///
/// The A/B is the assertion. Run alone, the first half only shows that *some*
/// configuration passes a big prompt through, which a hard-coded budget would
/// also do; run against the same prompt, the same loop and the same transport,
/// the only thing that differs is the pair the route derived — which is BR-1's
/// whole claim.
///
/// The second half runs the local *pair* rather than the local *engine*
/// deliberately: swapping the tier as well would leave two variables moving,
/// and AC-2's claim is about the budget, not about who serves the turn.
#[tokio::test]
async fn a_128k_route_assembles_a_20000_word_prompt_whole_and_the_default_pair_clamps_it() {
    const WORDS: usize = 20_000;
    let prompt = distinct_words(WORDS);
    assert_eq!(prompt.split_whitespace().count(), WORDS);
    // The generator's shape, stated rather than assumed: 3 characters and a
    // separator per word. It is not a claim about which guard binds — that is
    // the `context_budget_bytes` assertion below, and it is the one that would
    // have to move if this density ever did.
    assert_eq!(prompt.len(), WORDS * 4 - 1);

    // ---- the 128k route: whole, one request, quiet ----
    let config = route_config(128_000);
    assert!(
        config.context_budget_tokens > WORDS,
        "a 128k window must budget more than 20,000 words, or this proves nothing"
    );
    assert!(
        config.context_budget_bytes > prompt.len(),
        "and more than the fixture's bytes, so the word budget is what is under test"
    );

    let transport = ScriptedSseTransport::with_bodies(vec![sse_turn(&["ack"], None, 21_000, 2)]);
    let egress = Egress::new(
        transport.clone(),
        Vec::new(),
        Arc::new(tetond::egress::NoopSink),
    )
    .with_cost_meter(ledger());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "kimi",
        "https://api.moonshot.ai/v1/chat/completions",
    ));
    let session_id = SessionId::from("wide-window");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "kimi",
        "kimi-k2",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );
    let rig = loop_rig(&session_id);
    let mut sub = rig.bus.subscribe(256);
    let mut ctx = seeded(
        &config,
        build_system_prompt(&rig.tools, &config),
        prompt.as_str(),
    );
    drive(&mut source, &rig, &mut ctx, &config)
        .await
        .expect("a prompt inside the route's budget must complete");

    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "one request, not a compacted retry");
    let sent = requests[0]["messages"]
        .as_array()
        .expect("the adapter sends a messages array")
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        sent.contains(&prompt),
        "the 20,000-word prompt did not reach the provider whole: {} bytes of \
         message content against a {}-byte prompt",
        sent.len(),
        prompt.len()
    );
    let quiet = collect_events(&mut sub).await;
    assert!(
        pressure_events(&quiet).is_empty(),
        "nothing was dropped or elided, so nothing may be announced"
    );
    assert!(
        !streamed_text(&quiet).contains("did not fit"),
        "and the turn's own output carries no clamp notice either"
    );

    // ---- the default pair: the same prompt, clamped and announced ----
    let local = HarnessConfig::default();
    assert!(
        local.context_budget_bytes < prompt.len(),
        "the fixture must not fit the default pair, or the clamp never fires"
    );
    let transport = ScriptedSseTransport::with_bodies(vec![sse_turn(&["ack"], None, 4_000, 2)]);
    let egress = Egress::new(
        transport.clone(),
        Vec::new(),
        Arc::new(tetond::egress::NoopSink),
    )
    .with_cost_meter(ledger());
    let session_id = SessionId::from("narrow-window");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "kimi",
        "kimi-k2",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );
    let rig = loop_rig(&session_id);
    let mut sub = rig.bus.subscribe(256);
    let mut ctx = seeded(
        &local,
        build_system_prompt(&rig.tools, &local),
        prompt.as_str(),
    );
    // REQ-618 BR-1 changed what "bounded" means on this half. The prompt is the
    // turn's ask, and an ask may not be shortened, so the narrow pair no longer
    // clamps it and sends a shortened question — it refuses the turn outright.
    // The bound is therefore enforced *harder* than before: not "a smaller
    // request reaches the provider" but "no request reaches it at all".
    let err = drive(&mut source, &rig, &mut ctx, &local)
        .await
        .expect_err("the default pair must refuse a 20,000-word ask, not shorten it");
    let HarnessError::AnchorsExceedBudget {
        anchor_bytes,
        budget_bytes,
        anchor_kinds,
        ..
    } = &err
    else {
        panic!("the narrow pair must refuse by name: {err}");
    };
    assert!(*anchor_bytes > *budget_bytes, "{err}");
    assert_eq!(anchor_kinds, "user_ask");
    assert_eq!(
        *budget_bytes, local.context_budget_bytes,
        "the refusal carries the byte budget the gate enforced"
    );
    assert!(
        transport.requests().is_empty(),
        "the default pair sent something — the budget bounded nothing: {} request(s)",
        transport.requests().len()
    );

    // BR-7's "nothing in silence" still holds, through the refusal rather than
    // through an elision notice: the event names both figures and what could
    // not be given up, and it never quotes what would not fit.
    let published = collect_events(&mut sub).await;
    let refusals: Vec<_> = published
        .iter()
        .filter_map(|e| match &e.event {
            Event::TurnRefusedAnchorsExceedBudget(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "one refusal, announced once: {published:#?}"
    );
    assert_eq!(refusals[0].budget_bytes, local.context_budget_bytes as u64);
    assert_eq!(refusals[0].anchor_kinds, vec!["user_ask".to_owned()]);
    assert!(
        pressure_events(&published)
            .iter()
            .all(|p| !p.newest_user_elided),
        "nothing was clamped, so nothing may say it was"
    );
    let streamed = streamed_text(&published);
    assert!(
        !streamed.contains(&prompt[..64]),
        "no surface quotes what would not fit"
    );
}

/// **AC-3's typed outcome, at the loop.** A provider that answers 400 with a
/// context-length body ends the turn with
/// [`HarnessError::ContextLengthExceeded`] carrying both numbers, after
/// **exactly one** request.
///
/// The variant matters as much as the numbers: the daemon's fallback arm
/// matches `HarnessError::Remote`, so a refusal that arrived wearing that shape
/// would be retried against a fallback provider and would cost the refusing
/// provider a health downgrade. Asserted here rather than left to the runtime,
/// because this is the frame that decides it.
#[tokio::test]
async fn a_context_length_refusal_ends_the_turn_typed_after_one_request() {
    let transport = RefuseOversized::new(KIMI_TOO_LONG);
    let egress = Egress::new(
        transport.clone(),
        Vec::new(),
        Arc::new(tetond::egress::NoopSink),
    )
    .with_cost_meter(ledger());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "kimi",
        "https://api.moonshot.ai/v1/chat/completions",
    ));
    let session_id = SessionId::from("too-big");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "kimi",
        "kimi-k2",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    // A window the daemon believes is 128k while the provider disagrees — the
    // shape a wrong `capabilities.max_context` or a denser-than-estimated
    // prompt actually takes.
    let config = route_config(128_000);
    let rig = loop_rig(&session_id);
    let mut ctx = seeded(
        &config,
        build_system_prompt(&rig.tools, &config),
        &distinct_words(2_000),
    );
    let err = drive(&mut source, &rig, &mut ctx, &config)
        .await
        .expect_err("a refused turn must not report success");

    match &err {
        HarnessError::ContextLengthExceeded {
            provider_id,
            assembled_tokens,
            budget_tokens,
        } => {
            assert_eq!(provider_id, "kimi");
            assert_eq!(*budget_tokens, config.context_budget_tokens);
            assert!(
                *assembled_tokens >= 2_000,
                "the report must name what was actually assembled, got {assembled_tokens}"
            );
        }
        other => panic!("expected the typed refusal, got {other:?}"),
    }
    assert!(
        !matches!(err, HarnessError::Remote(_)),
        "a context-length refusal must not wear the shape the daemon retries \
         and downgrades health for"
    );
    assert_eq!(
        transport.calls(),
        1,
        "no retry: resending the same bytes cannot succeed"
    );
    // Content-free, per conventions.md: the provider's body never rides the
    // error out.
    let sentence = err.to_string();
    assert!(
        !sentence.contains("Input token length"),
        "the provider's own prose leaked into the error: {sentence}"
    );
}
