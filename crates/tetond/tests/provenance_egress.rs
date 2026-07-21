//! Provenance-from-files-touched egress enforcement (REQ-544 C-1 + M-2).
//!
//! The BR-1 bypass this suite guards against: before REQ-544, a tool result's
//! egress provenance came from a literal `path` argument, so `shell`, `grep`, and
//! `glob` — which surface boundary-file content without a `path` arg — folded
//! into context with EMPTY provenance and could be laundered to a remote provider
//! on the next turn with no `privacy_block`.
//!
//! Each test drives the **real** OpenAI-compatible adapter through the **real**
//! egress choke point in front of a capture transport. The scripted remote model
//! reads a `local-only` file via `shell`/`grep`/`glob`; the loop folds that result
//! with the provenance of the files the tool *actually touched* (or UNKNOWN for
//! `shell`), and the *next* remote turn is blocked before a byte leaves. The
//! tests assert:
//!
//! 1. the turn is blocked (a privacy block, not a silent leak),
//! 2. zero boundary bytes reached the capture transport,
//! 3. exactly one `privacy_block` event fired, and
//! 4. the built-in tool result was framed as untrusted content (M-2).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::Event;
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::egress::Egress;
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn_with_source, ContextManager,
    HarnessConfig, HarnessError, NoopProvenanceHook, PendingPermissions, PermissionConfig,
    PermissionGate, RemoteProviderSource, SessionEvents, ToolContext, ToolRegistry,
};

/// The boundary-file secret that must never reach the capture transport.
const SECRET: &str = "API_KEY=sk-live-DO-NOT-LEAK-provctl-Zx9";

/// A capturing OpenAI-compatible SSE transport: returns a queue of canned bodies
/// (one per turn) and records every request body it was asked to send.
#[derive(Clone, Default)]
struct CaptureSse {
    bodies: Arc<Mutex<VecDeque<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    calls: Arc<AtomicUsize>,
}

impl CaptureSse {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            sent: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn captured(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for CaptureSse {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sent.lock().unwrap().push(request.body.clone());
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

/// One OpenAI-compatible streaming turn: a text delta, an optional tool call,
/// then usage + `[DONE]`.
fn sse_turn(text: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": id, "function": { "name": name, "arguments": args }
            }]}}]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    let usage = serde_json::json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 5 } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

fn temp_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-provctl-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub const A: u32 = 1;\n").unwrap();
    std::fs::write(root.join("secrets/prod.env"), format!("{SECRET}\n")).unwrap();
    root
}

fn boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Drive the loop: turn 1 is the scripted tool call that touches the boundary
/// file; turn 2 (which would carry the result to the wire) must be blocked.
/// Returns the loop result, the captured request bodies, the `privacy_block`
/// events, and the assembled context (to inspect provenance + framing).
async fn run_touching_tool(
    repo: &std::path::Path,
    tool: (&str, &str, &str),
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    Vec<teton_protocol::events::PrivacyBlock>,
    ContextManager,
) {
    // Turn 1: the tool call. Turn 2 is scripted but must never be reached.
    let transport = CaptureSse::with_bodies(vec![
        sse_turn("Reading the config.", Some(tool)),
        sse_turn("should never send", None),
    ]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(transport, boundaries(), bus.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));

    let session_id = SessionId::from("provctl");
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
    );

    // Full profile so the loop does not force a verification nudge before the
    // second (blocked) turn.
    let config = HarnessConfig::for_strong_model();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(repo);
    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("Read the production config and summarize it.");

    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
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

    // Drain privacy_block events.
    let mut blocks = Vec::new();
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::PrivacyBlock(pb) = env.event {
            blocks.push(pb);
        }
    }
    (result, capture.captured(), blocks, ctx)
}

/// Assert the shared BR-1 guarantee for a boundary-touching built-in tool.
fn assert_blocked_and_clean(
    result: &Result<tetond::harness::TurnOutcome, HarnessError>,
    captured: &[Vec<u8>],
    blocks: &[teton_protocol::events::PrivacyBlock],
) {
    // (1) The turn was blocked as a privacy block — not a leak, not a generic
    // transport retry.
    match result {
        Err(e) if e.is_privacy_block() => {}
        other => panic!("expected a privacy block, got {other:?}"),
    }
    // (2) Zero boundary bytes reached the wire in ANY captured request.
    for body in captured {
        assert!(
            !contains_bytes(body, SECRET),
            "boundary content leaked into captured egress"
        );
    }
    // (3) Exactly one privacy_block event (REQ-544 M-1 — no duplicate blocks).
    assert_eq!(blocks.len(), 1, "exactly one privacy_block");
}

/// The last tool-result block folded into `ctx`, framed as untrusted (M-2).
fn assert_last_tool_result_is_framed(ctx: &ContextManager) {
    use tetond::harness::context::Provenance;
    let framed = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("a tool result was folded into context");
    assert!(
        framed.contains("trust=\"untrusted\""),
        "built-in tool result must be framed as untrusted (M-2): {framed}"
    );
}

// ---------------------------------------------------------------------------
// shell — UNKNOWN provenance, fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_cat_of_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) = run_touching_tool(
        &repo,
        ("c1", "shell", r#"{"command":"cat secrets/prod.env"}"#),
    )
    .await;

    // A shell result cannot be attributed to a file set, so it is UNKNOWN — and
    // the context is therefore unknown-provenance, which egress fail-closes.
    assert!(
        context_provenance(&ctx).is_unknown(),
        "a shell result must taint the context as unknown provenance"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// grep — matched-files provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_matching_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "grep", r#"{"pattern":"sk-live"}"#)).await;

    // grep tagged the result with the matched boundary file.
    assert!(
        context_provenance(&ctx).contains("secrets/prod.env"),
        "grep must tag the result with the matched boundary file"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_eq!(blocks[0].path, "secrets/prod.env");
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// glob — enumerated-files provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_enumerating_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "glob", r#"{"pattern":"secrets/**"}"#)).await;

    assert!(
        context_provenance(&ctx).contains("secrets/prod.env"),
        "glob must tag the result with the enumerated boundary file"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_eq!(blocks[0].path, "secrets/prod.env");
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}
