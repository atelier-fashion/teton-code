//! Real-engine smoke test — compiled ONLY under `--features llama`.
//!
//! The whole file is gated on the `llama` feature, so under default features
//! (and therefore in CI) it compiles to an empty test binary: llama.cpp is never
//! built. Even with the feature enabled the test is `#[ignore]`d because it needs
//! a real GGUF on disk, pointed to by `TETON_TEST_GGUF`. Run it explicitly:
//!
//! ```text
//! TETON_TEST_GGUF=/path/to/model.gguf \
//!   cargo test -p teton-inference --features llama --test llama_smoke -- --ignored
//! ```
#![cfg(feature = "llama")]

use teton_inference::engine::{Engine, GenParams, LlamaEngine};

#[test]
#[ignore = "requires a real GGUF at $TETON_TEST_GGUF and cmake to build llama.cpp"]
fn llama_engine_streams_a_completion() {
    let path =
        std::env::var("TETON_TEST_GGUF").expect("set TETON_TEST_GGUF to a local GGUF model file");
    // Offload everything to the GPU (the Metal fast path on Apple Silicon).
    let engine =
        LlamaEngine::load("smoke", std::path::Path::new(&path), u32::MAX, 2048).expect("load GGUF");

    let mut streamed = String::new();
    let completion = engine
        .complete(
            "Reply with the single word: ready.",
            &GenParams {
                max_tokens: 8,
                temperature: 0.0,
            },
            &mut |token| streamed.push_str(token),
        )
        .expect("completion succeeds");

    assert!(completion.completion_tokens > 0);
    assert_eq!(streamed, completion.text);
}
