//! REQ-554 AC-6 — one real agent turn through the model's **native chat
//! template**, on real weights.
//!
//! Everything else about REQ-554 is pinned by the always-on suite (ADR-1 keeps
//! the renderer in pure Rust precisely so AC-1/AC-2/AC-5/AC-8 need no weights).
//! What that suite cannot tell you is whether a real instruct model, shown the
//! ChatML framing instead of the flat `User:`/`Assistant:` transcript, actually
//! emits **one well-formed tool call** — the fidelity claim the whole REQ rests
//! on. Mock-only green is explicitly not acceptance here (LESSON-433).
//!
//! The turn is driven through [`LocalEngineSource`], not through a hand-rolled
//! prompt, so this exercises the production path end to end: the source caches
//! the engine's [`ChatFormat`], renders through `harness::render::render_prompt`,
//! scans the stream with the ChatML marker set, and parses the cut text. Those
//! items are `pub(crate)`; reaching them from an integration test would mean
//! widening the harness's API for a test, so the test drives the public seam
//! that calls them instead — which is the better check anyway, since a wiring
//! regression is exactly what it would catch.
//!
//! The whole file is gated on the `llama` feature, so a default build (and CI)
//! compiles it to an empty test binary and never builds llama.cpp. Even with the
//! feature on it is `#[ignore]`d: it needs real weights on disk. Run it
//! explicitly:
//!
//! ```text
//! TETON_TEST_GGUF=/path/to/qwen.gguf \
//!   cargo test -p tetond --features llama --test template_smoke -- --ignored --nocapture
//! ```
#![cfg(feature = "llama")]

use std::path::Path;
use std::sync::{Arc, Mutex};

use teton_inference::{ChatFormat, Engine, LlamaEngine};
use teton_protocol::SessionId;

use tetond::egress::Provenance as EgressProvenance;
use tetond::harness::{
    build_system_prompt, CompletionSource, ContextManager, HarnessConfig, LocalEngineSource,
    NoopProvenanceHook, ToolRegistry, TurnDecision,
};

/// The generation window the daemon loads the local tier with
/// (`runtime::LOCAL_ENGINE_N_CTX`), mirrored here so the smoke runs the same
/// shape production does.
const SMOKE_N_CTX: u32 = 32_768;

#[tokio::test]
#[ignore = "requires real weights at $TETON_TEST_GGUF and cmake to build llama.cpp"]
async fn a_templated_turn_yields_one_well_formed_tool_call() {
    let path =
        std::env::var("TETON_TEST_GGUF").expect("set TETON_TEST_GGUF to a local GGUF model file");
    // Offload everything to the GPU (the Metal fast path on Apple Silicon), as
    // the real loader does on a GPU-classed machine.
    let engine = LlamaEngine::load("template-smoke", Path::new(&path), u32::MAX, SMOKE_N_CTX)
        .expect("load GGUF");

    // BR-1: the catalog family embeds a ChatML template, and the engine must
    // recognize it. If this fails, the turn below would silently run on the flat
    // fallback and the rest of the test would prove nothing.
    assert_eq!(
        engine.chat_format(),
        ChatFormat::ChatMl,
        "the GGUF at $TETON_TEST_GGUF carries no template this build recognizes — \
         the fallback would make the rest of this smoke vacuous"
    );

    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let exposed = tools.exposed_names(config.max_tools);

    // The same conversation an agent turn starts from: the harness's real system
    // prompt (which teaches the inline-JSON tool protocol) plus one user request
    // that can only be answered by calling `read`.
    let mut ctx = ContextManager::new(build_system_prompt(&tools, &config), 4_096);
    ctx.push_user("Read the file src/main.rs so you can tell me what it does. Use a tool.");
    let prompt = ctx.prepare(&mut NoopProvenanceHook);

    // The format is passed the way the daemon's engine slot passes it —
    // resolved from the engine at install, never locked for on the async path.
    // The session id is the prefix cache's key (REQ-564 BR-3); this smoke
    // drives one turn in one session, so any fixed id is faithful.
    let format = engine.chat_format();
    let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(engine));
    let mut source = LocalEngineSource::new(engine, format, SessionId::from("template-smoke"));
    assert_eq!(source.chat_format(), ChatFormat::ChatMl);

    let mut streamed = String::new();
    let turn = source
        .produce_turn(
            &prompt,
            &EgressProvenance::empty(),
            &config,
            &tools,
            &exposed,
            &mut |token| streamed.push_str(token),
        )
        .await
        .expect("the local turn completes");

    // AC-6: exactly one well-formed call. `dropped_calls` is how the harness
    // reports extras, so a model that ran on and emitted a second call fails
    // here rather than quietly having the first one kept.
    assert_eq!(
        turn.dropped_calls, 0,
        "the model emitted more than one tool call; raw stream was:\n{streamed}"
    );
    match &turn.decision {
        TurnDecision::ToolCall { name, arguments } => {
            assert_eq!(name, "read", "raw stream was:\n{streamed}");
            let args = arguments
                .as_object()
                .expect("a well-formed call carries an argument object");
            assert!(
                args.get("path").and_then(|p| p.as_str()).is_some(),
                "the `read` call carries no string `path`: {arguments}"
            );
        }
        other => panic!("expected one tool call, got {other:?}; raw stream was:\n{streamed}"),
    }

    // BR-3/BR-4: the containment ran in template mode. The text kept for context
    // is cut at the end of the call — a fabricated `<|im_start|>` continuation,
    // the template-mode analogue of BUG-147's fake `Tool (read):` blocks, never
    // survives into context.
    assert!(
        !turn.text.contains("<|im_start|>"),
        "a fabricated ChatML turn reached context: {}",
        turn.text
    );
}
