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

use async_trait::async_trait;

use teton_protocol::events::{Event, SessionUpdatePayload};
use teton_protocol::methods::StopReason;
use teton_protocol::{Phase, SessionId};
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::Egress;
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, HarnessConfig, HarnessError,
    NoopProvenanceHook, PendingPermissions, PermissionConfig, PermissionGate, RemoteProviderSource,
    SessionEvents, ToolContext, ToolRegistry,
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
}

impl ScriptedSseTransport {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Transport for ScriptedSseTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
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
        None,
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
        Some("secrets/prod.env".to_owned()),
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
        None,
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
