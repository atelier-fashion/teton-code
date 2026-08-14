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

use teton_inference::{ChatFormat, Completion, Engine, EngineError, GenParams};
use teton_protocol::methods::StopReason;
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::harness::context::Provenance;
use tetond::harness::shell_duty::SHELL_OUTPUT_CONTRACT;
use tetond::harness::tools::DOCS_TOOL_NAME;
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn, ContextManager, HarnessConfig,
    PendingPermissions, PermissionConfig, PermissionGate, RecordingProvenanceHook, SessionEvents,
    ToolContext, ToolRegistry,
};
use tetond::provider_recipes::recipe_catalog;

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
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        // **A duty is not a turn** (REQ-561 BR-10). The daemon also makes model
        // calls on its own behalf, and this script is a sequence of *turns* —
        // `calls` is what the tests below read as the turn count. Serving a duty
        // from the script would shift every reply after it by one and make that
        // count mean something else, which is the desync REQ-558 shipped twice.
        //
        // `shell` is the one that reaches here: the offline entry point resolves
        // it onto this same engine, and a *deliberately failing* verify — the
        // whole subject of `a_failing_verify_after_an_edit_does_not_satisfy_the_gate`
        // — is exactly what it fires on. Recognized by its own output contract
        // and answered off-script, consuming no reply and no count.
        // The fifth duty, `compact`, does **not** reach here today: these
        // fixtures' contexts stay far under the soft pressure threshold, so it
        // declines without a model call (verified by making this assertion fire
        // and watching it not). It gets an assertion rather than an answer,
        // because inventing a compaction for a fixture that never needed one
        // would rewrite that fixture's history for no reason — while a fixture
        // that GREW past the threshold and silently ate a scripted block is the
        // desync BR-10 exists to prevent. So the day one does, this says so.
        assert!(
            !prompt.contains(tetond::harness::compact::COMPACT_OUTPUT_CONTRACT),
            "a fixture here now crosses the compaction threshold, so the `compact` duty \
             reaches this stand-in engine — give it an arm of its own before it eats a \
             scripted reply block (REQ-561 BR-10)"
        );
        let text = if prompt.contains(SHELL_OUTPUT_CONTRACT) {
            "The command exited non-zero, so the change is not verified.".to_owned()
        } else {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies
                .get(idx)
                .cloned()
                .unwrap_or_else(|| "Done.".to_owned())
        };

        let full = text;
        let mut text = String::new();
        let mut completion_tokens = 0u32;
        for token in full.split_inclusive(' ') {
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
        &engine,
        ChatFormat::Flat,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
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
        &engine,
        ChatFormat::Flat,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
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
        &engine,
        ChatFormat::Flat,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
    )
    .await
    .expect("loop terminates");

    // It stopped at the ceiling rather than running forever.
    assert_eq!(outcome.stop_reason, StopReason::MaxTurnRequests);
    assert_eq!(outcome.turns, 4);

    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// REQ-577 AC-5 / AC-6 — the bundled-docs tool on the offline path
// ---------------------------------------------------------------------------

/// **REQ-577 BR-6/BR-7 (AC-5, AC-6): a session with no network answers a Teton
/// setup question out of this binary.**
///
/// The offline path is where that claim is worth making, for two reasons. It is
/// the session shape a first-run user actually has when they ask how to connect
/// a provider — there is no provider yet to ask — and it is the one where "zero
/// egress" is a fact about the *code* rather than about this fixture's luck:
/// [`run_session_turn`] takes no `Transport`, no provider and no network handle,
/// exactly as the top of this file describes.
///
/// **Non-vacuous by pairing (LESSON-520).** A zero-egress assertion over a turn
/// that served an *empty* result would pass while the feature was broken, so the
/// positive half here asserts the body the model received carries every endpoint
/// and example model the recipe catalog ships. Those values are read from
/// [`recipe_catalog`] rather than typed out again: a recipe that moves fails
/// this test instead of quietly turning its own assertion into a tautology, and
/// this file is deliberately *not* a place where a second spelling of a vendor's
/// endpoint lives (BR-2 keeps that to the catalog and its golden).
#[tokio::test]
async fn an_offline_session_serves_a_teton_docs_topic_with_zero_egress() {
    let repo = temp_repo();

    // Turn 1 asks the binary; turn 2 hands the user the commands and stops. No
    // `read`, no `grep`, no `shell` — the repository is never consulted, which
    // is the behavior BUG-160 could not produce for lack of a docs tool.
    let script = [
        r#"Teton's own docs carry the exact commands.
{"tool": "teton_docs", "arguments": {"topic": "providers"}}"#,
        "Register the provider with `teton provider add`, then point a tier at it with \
         `teton policy set-tier`. Both are yours to run — I cannot run them for you.",
    ];

    let calls = Arc::new(AtomicUsize::new(0));
    let engine: Arc<Mutex<dyn Engine>> =
        Arc::new(Mutex::new(ScriptedEngine::new(&script, Arc::clone(&calls))));

    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);

    let system = build_system_prompt(&tools, &config);
    // AC-5's exposure half, on the profile this file drives. A model can only
    // call what the prompt named, so a docs call the loop happily serves but the
    // prompt never advertised would make the rest of this test a statement about
    // a fixture rather than about a session (BR-7: the one unacceptable outcome
    // is *silently absent*).
    assert!(
        system.contains(DOCS_TOOL_NAME),
        "`{DOCS_TOOL_NAME}` is not exposed in the offline session's system prompt"
    );

    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("How do I hook up Kimi for deep reasoning?");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let session_id = SessionId::from("offline-docs-1");
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let mut hook = RecordingProvenanceHook::default();

    let outcome = run_session_turn(
        &engine,
        ChatFormat::Flat,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
    )
    .await
    .expect("local turn completes");

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(
        !outcome.edited,
        "a docs read edits nothing, so the mandatory-verification shape is not in play"
    );

    // --- the positive half: what the model actually received -----------------
    let served = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("the docs result was folded into context");

    let catalog = recipe_catalog();
    assert_eq!(
        catalog.len(),
        6,
        "the recipe roster changed; this sweep is narrower than the catalog"
    );
    for recipe in &catalog {
        if let Some(endpoint) = &recipe.endpoint {
            assert!(
                served.contains(endpoint.as_str()),
                "the served `providers` topic carries no endpoint for {}: {served}",
                recipe.label
            );
        }
        assert!(
            served.contains(recipe.example_model.as_str()),
            "the served `providers` topic carries no example model for {}: {served}",
            recipe.label
        );
    }

    // --- the negative half: nothing left, and nothing was touched ------------
    // Every model call was served by the LOCAL scripted engine (two turns: the
    // docs call, then the answer), and there is no transport in the loop's
    // signature to have reached a provider with.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the local engine served every turn"
    );
    assert!(!hook.seen.is_empty());
    assert!(hook.seen.iter().all(|p| matches!(
        p,
        Provenance::System | Provenance::User | Provenance::Model | Provenance::Tool { .. }
    )));

    // The context the *next* turn would carry names no source at all: the body
    // came out of the binary, so there is no identity to mint. `Unknown` would
    // be the other failure — a docs read that fail-closed every later remote
    // turn over the daemon's own documentation.
    let provenance = context_provenance(&ctx);
    assert!(
        !provenance.is_unknown(),
        "a bundled body has knowable provenance"
    );
    assert_eq!(
        provenance.len(),
        0,
        "`{DOCS_TOOL_NAME}` opened no path: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );

    // And the repository really was left alone — the file this fixture ships is
    // the one thing a tool that had gone looking would have had to open.
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub const ANSWER: u32 = 1;\n",
        "the session touched the repository"
    );

    std::fs::remove_dir_all(&repo).ok();
}
