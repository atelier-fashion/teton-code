//! Offline freeform session — the AC-1 core (architecture D-3, local-first).
//!
//! A freeform session drives a full **read → edit → verify** flow against a
//! *local* engine mock and completes with **zero egress**. Zero egress is not an
//! assertion bolted on after the fact: [`run_session_turn`] takes no
//! `Transport`, no provider, and no network handle, so the local path *cannot*
//! reach the network by construction. This test exercises that path end to end
//! and confirms:
//!
//! - the file on disk is actually edited (not a silent no-op),
//! - the loop performed the mandatory post-edit verification (BR-6 weak-model
//!   shape),
//! - every model call was served by the local engine (a scripted mock), and
//! - the provenance seam that TASK-007's egress plugs into saw only
//!   local-origin content.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use teton_inference::{Completion, Engine, EngineError, GenParams};
use teton_protocol::methods::StopReason;
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::harness::context::Provenance;
use tetond::harness::{
    build_system_prompt, run_session_turn, ContextManager, HarnessConfig, PendingPermissions,
    PermissionConfig, PermissionGate, RecordingProvenanceHook, SessionEvents, ToolContext,
    ToolRegistry,
};

/// A local [`Engine`] that replays a fixed script of replies, one per turn, and
/// counts how many times it was called. When the script is exhausted it returns
/// a plain-text end-of-turn — so a runaway loop cannot outrun the mock.
struct ScriptedEngine {
    replies: Vec<String>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedEngine {
    fn new(replies: &[&str], calls: Arc<AtomicUsize>) -> Self {
        Self {
            replies: replies.iter().map(|s| (*s).to_owned()).collect(),
            calls,
        }
    }
}

impl Engine for ScriptedEngine {
    fn model_id(&self) -> &str {
        "scripted-local"
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, EngineError> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .replies
            .get(idx)
            .cloned()
            .unwrap_or_else(|| "Done.".to_owned());

        let mut completion_tokens = 0u32;
        for token in text.split_inclusive(' ') {
            if completion_tokens >= params.max_tokens {
                break;
            }
            on_token(token);
            completion_tokens += 1;
        }
        let prompt_tokens = u32::try_from(prompt.split_whitespace().count()).unwrap_or(u32::MAX);
        Ok(Completion {
            text,
            prompt_tokens,
            completion_tokens,
        })
    }
}

/// Create an isolated temp repo containing `src/lib.rs`.
fn temp_repo() -> PathBuf {
    // A process-wide counter guarantees uniqueness even when two tests run in
    // parallel within the same nanosecond (which would otherwise let one test's
    // cleanup delete another's repo).
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-offline-{}-{}-{}",
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

#[tokio::test]
async fn offline_read_edit_verify_completes_with_zero_egress() {
    let repo = temp_repo();

    // The local model's scripted plan: read the file, edit the constant, verify
    // the edit with a shell grep, then finish. No network is involved anywhere.
    let script = [
        r#"I'll read the file first.
{"tool": "read", "arguments": {"path": "src/lib.rs"}}"#,
        r#"Now change the constant.
{"tool": "edit", "arguments": {"path": "src/lib.rs", "old_string": "pub const ANSWER: u32 = 1;", "new_string": "pub const ANSWER: u32 = 2;"}}"#,
        r#"Verify the change landed.
{"tool": "shell", "arguments": {"command": "grep -q 'ANSWER: u32 = 2' src/lib.rs && echo VERIFIED"}}"#,
        "Done. src/lib.rs now defines ANSWER = 2 and the change is verified.",
    ];

    let calls = Arc::new(AtomicUsize::new(0));
    let engine: Arc<Mutex<dyn Engine>> =
        Arc::new(Mutex::new(ScriptedEngine::new(&script, Arc::clone(&calls))));

    let config = HarnessConfig::default(); // weak-model shape: verification required
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("In src/lib.rs change ANSWER from 1 to 2, then verify it.");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let session_id = SessionId::from("offline-1");
    // The operator has pre-approved the local, jailed tool set (the AC-1 demo
    // path) — so read/edit/verify run without a permission round-trip.
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let mut hook = RecordingProvenanceHook::default();

    // A subscriber proves the streaming turn surface actually broadcasts.
    let mut sub = bus.subscribe(256);

    let outcome = run_session_turn(
        &engine, &tools, &tool_ctx, &gate, &events, &mut ctx, &config, &mut hook,
    )
    .await
    .expect("local turn completes");

    // The turn ended cleanly on the model's end-of-turn, having edited AND
    // verified (weak-model mandatory-verification shape).
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(outcome.edited, "an edit should have landed");
    assert!(outcome.verified, "the edit should have been verified");

    // The edit really happened on disk.
    let updated = std::fs::read_to_string(repo.join("src/lib.rs")).unwrap();
    assert!(
        updated.contains("pub const ANSWER: u32 = 2;"),
        "file was not edited: {updated}"
    );

    // Zero egress: every model call was served by the LOCAL scripted engine
    // (four turns: read, edit, verify, finish), and there is no transport in the
    // loop's signature to reach a provider with.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "local engine served every turn"
    );

    // The provenance seam TASK-007 will use saw only local-origin content — no
    // block carried a remote destination (there is no such provenance on this
    // path).
    assert!(!hook.seen.is_empty());
    assert!(hook.seen.iter().all(|p| matches!(
        p,
        Provenance::System | Provenance::User | Provenance::Model | Provenance::Tool { .. }
    )));

    // The session broadcast streaming updates (agent messages + tool status).
    let mut session_updates = 0;
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if env.event_name() == "session_update" {
            session_updates += 1;
        }
    }
    assert!(session_updates > 0, "the turn should have streamed updates");

    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn a_failing_verify_after_an_edit_does_not_satisfy_the_gate() {
    // REQ-544 MED-4: the BR-6 verification gate is only satisfied by a verify
    // tool call that SUCCEEDED. Here the model edits the file, then runs a
    // verification step that FAILS (a non-zero shell exit). The failing check
    // must NOT flip `verified` true — the loop nudges the model to actually
    // verify, and the turn ends with `verified == false`.
    let repo = temp_repo();

    let script = [
        r#"Change the constant.
{"tool": "edit", "arguments": {"path": "src/lib.rs", "old_string": "pub const ANSWER: u32 = 1;", "new_string": "pub const ANSWER: u32 = 2;"}}"#,
        // A verification attempt that FAILS (non-zero exit → is_error). Under the
        // old code this still marked the edit verified; it must not now.
        r#"Verify the change.
{"tool": "shell", "arguments": {"command": "exit 3"}}"#,
        // First end-of-turn: the loop nudges once because the edit is unverified.
        "I believe the change is complete.",
        // Second end-of-turn after the nudge: the loop respects it and returns.
        "Done.",
    ];

    let calls = Arc::new(AtomicUsize::new(0));
    let engine: Arc<Mutex<dyn Engine>> =
        Arc::new(Mutex::new(ScriptedEngine::new(&script, Arc::clone(&calls))));

    let config = HarnessConfig::default(); // weak-model shape: verification required
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("In src/lib.rs change ANSWER from 1 to 2, then verify it.");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let session_id = SessionId::from("failing-verify-1");
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let mut hook = tetond::harness::NoopProvenanceHook;

    let outcome = run_session_turn(
        &engine, &tools, &tool_ctx, &gate, &events, &mut ctx, &config, &mut hook,
    )
    .await
    .expect("local turn completes");

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(outcome.edited, "the edit landed");
    assert!(
        !outcome.verified,
        "a FAILING verify tool must not satisfy the verification gate"
    );
    // The failing verify forced the one-shot nudge, so the model was asked to
    // verify again before the loop honored its end-of-turn (edit, fail-verify,
    // end→nudge, end).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "the failing verify should have triggered the mandatory-verification nudge"
    );

    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn malformed_tool_calls_do_not_cause_an_unbounded_loop() {
    let repo = temp_repo();

    // The model keeps emitting a call to a tool that does not exist. The loop
    // must fold the error back and remain bounded by max_turns, never spinning.
    let bad = r#"{"tool": "nonexistent_tool", "arguments": {}}"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(ScriptedEngine::new(
        &[bad, bad, bad, bad],
        Arc::clone(&calls),
    )));

    let config = HarnessConfig {
        max_turns: 4,
        require_verification: false,
        ..HarnessConfig::default()
    };
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("do something");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let session_id = SessionId::from("bounded-1");
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let mut hook = tetond::harness::NoopProvenanceHook;

    let outcome = run_session_turn(
        &engine, &tools, &tool_ctx, &gate, &events, &mut ctx, &config, &mut hook,
    )
    .await
    .expect("loop terminates");

    // It stopped at the ceiling rather than running forever.
    assert_eq!(outcome.stop_reason, StopReason::MaxTurnRequests);
    assert_eq!(outcome.turns, 4);

    std::fs::remove_dir_all(&repo).ok();
}
