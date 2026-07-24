//! Real local inference must ride the blocking pool, never a tokio worker.
//!
//! With a `ScriptedFileEngine` a completion is microseconds and running it
//! inline in the async turn loop went unnoticed. With a real `LlamaEngine` a
//! completion is *seconds*, so an inline call parks the tokio worker driving the
//! turn — with N concurrent sessions, N parked workers — and the whole daemon
//! stops answering every client's RPCs (the adversary-confirmed major finding of
//! the engine-wiring work; the consent gate's loader already followed the E-3
//! rule and the serving path now does too).
//!
//! Both tests run on a runtime with exactly ONE worker thread and an engine
//! whose `complete` blocks until released — the shape of real inference. While
//! the engine is provably mid-completion, an unrelated async task (another
//! client's RPC, in daemon terms) must still run to completion. Under the old
//! inline code the sole worker is parked inside `complete`, the unrelated task
//! cannot run until inference ends, and the `is_finished` assertions fail.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teton_inference::{Completion, Engine, EngineError, GenParams};
use teton_protocol::methods::StopReason;
use teton_protocol::SessionId;

use tetond::broadcast::EventBus;
use tetond::harness::context::summarize_if_large;
use tetond::harness::{
    build_system_prompt, run_session_turn, ContextManager, HarnessConfig, NoopProvenanceHook,
    PendingPermissions, PermissionConfig, PermissionGate, SessionEvents, ToolContext, ToolRegistry,
};

/// How long a [`GatedEngine`] waits to be released before completing anyway.
///
/// The fallback exists so a regression FAILS instead of deadlocking: under
/// inline inference the sole worker is parked inside `complete`, the test body
/// (which holds the release sender) can never run, and without this bound the
/// test would hang forever.
const GATE_FALLBACK: Duration = Duration::from_secs(5);

/// An [`Engine`] whose `complete` blocks — like real llama.cpp inference — until
/// the test releases it (or [`GATE_FALLBACK`] elapses). It signals `started` the
/// moment it begins, so the test can act at a point where inference is provably
/// in flight.
struct GatedEngine {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    reply: String,
}

impl Engine for GatedEngine {
    fn model_id(&self) -> &str {
        "gated-local"
    }

    fn complete(
        &self,
        _prompt: &str,
        _params: &GenParams,
        _on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, EngineError> {
        self.started.send(()).ok();
        let _ = self.release.recv_timeout(GATE_FALLBACK);
        Ok(Completion {
            text: self.reply.clone(),
            prompt_tokens: 1,
            completion_tokens: 1,
        })
    }
}

/// A gated engine plus the test's ends of its two rendezvous channels.
fn gated_engine(reply: &str) -> (Arc<Mutex<dyn Engine>>, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(GatedEngine {
        started: started_tx,
        release: release_rx,
        reply: reply.to_owned(),
    }));
    (engine, started_rx, release_tx)
}

/// Block (off the runtime) until the engine reports it is mid-`complete`.
async fn engine_is_mid_completion(started: mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || started.recv())
        .await
        .expect("the started-signal wait must not panic")
        .expect("the engine must signal that inference began");
}

/// Prove the sole worker thread is free: an unrelated task runs to completion
/// while inference is still in flight.
async fn unrelated_task_completes(while_doing: &str) {
    let unrelated = tokio::spawn(async { 21 * 2 });
    let answer = tokio::time::timeout(Duration::from_secs(2), unrelated)
        .await
        .unwrap_or_else(|_| panic!("an unrelated task must complete while {while_doing}"))
        .expect("the unrelated task must not panic");
    assert_eq!(answer, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_slow_local_turn_does_not_stall_an_unrelated_task() {
    let (engine, started, release) = gated_engine("Done.");

    // Drive a full local turn on its own task, exactly as the daemon drives one
    // client's prompt. Everything the turn needs moves into the task.
    let turn = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move {
            let config = HarnessConfig::default();
            let tools = ToolRegistry::with_builtins();
            let tool_ctx = ToolContext::new(std::env::temp_dir());
            let system = build_system_prompt(&tools, &config);
            let mut ctx = ContextManager::new(system, config.context_budget_tokens);
            ctx.push_user("say done");
            let bus = Arc::new(EventBus::new());
            let session_id = SessionId::from("nonblocking-turn");
            let gate = PermissionGate::new(
                session_id.clone(),
                PermissionConfig::permissive(),
                Arc::clone(&bus),
                Arc::new(PendingPermissions::new()),
            );
            let events = SessionEvents::new(bus, session_id);
            let mut hook = NoopProvenanceHook;
            run_session_turn(
                &engine, &tools, &tool_ctx, &gate, &events, &mut ctx, &config, &mut hook,
            )
            .await
        }
    });

    engine_is_mid_completion(started).await;
    unrelated_task_completes("a local inference turn is in flight").await;

    // The order of proof matters: had inference been inline, the worker only
    // freed up because the engine's fallback ELAPSED — i.e. the turn already
    // finished. Mid-flight, the turn must still be running here.
    assert!(
        !turn.is_finished(),
        "the turn finished before it was released: inference ran inline on the \
         worker and the unrelated task only ran after it ended"
    );

    release.send(()).ok();
    let outcome = turn
        .await
        .expect("the turn task must not panic")
        .expect("the gated turn completes once released");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_slow_summarization_does_not_stall_an_unrelated_task() {
    let (engine, started, release) = gated_engine("CONDENSED");

    // An oversized tool result forces the summarization path.
    let summarize = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move {
            let big = "word ".repeat(500);
            summarize_if_large(&engine, "grep", &big, 50).await
        }
    });

    engine_is_mid_completion(started).await;
    unrelated_task_completes("a tool-result summarization is in flight").await;

    assert!(
        !summarize.is_finished(),
        "summarization finished before it was released: it ran inline on the \
         worker and the unrelated task only ran after it ended"
    );

    release.send(()).ok();
    let out = summarize.await.expect("the summarize task must not panic");
    assert!(out.text.contains("summarized grep output"));
    assert!(out.text.contains("CONDENSED"));
    assert_eq!(out.engine_error, None);
}
