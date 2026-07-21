//! The [`Engine`] abstraction over local inference.
//!
//! Everything above this trait — probe, download, benchmark, pressure — is
//! backend-agnostic and tests against [`MockEngine`]. The real llama.cpp binding
//! lives in [`LlamaEngine`], compiled only under the non-default `llama` feature
//! so that default builds and CI never pull in llama.cpp or cmake (see the crate
//! docs). The daemon selects the backend at runtime.

/// Parameters for a single completion request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenParams {
    /// Hard cap on generated tokens.
    pub max_tokens: u32,
    /// Sampling temperature; `0.0` is greedy.
    pub temperature: f32,
}

impl Default for GenParams {
    fn default() -> Self {
        // Local-tier duties (classification, summarization) want short, nearly
        // deterministic output.
        Self {
            max_tokens: 256,
            temperature: 0.2,
        }
    }
}

/// The result of a completion, with token accounting for the cost ledger and
/// the benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The full generated text.
    pub text: String,
    /// Tokens in the prompt.
    pub prompt_tokens: u32,
    /// Tokens generated.
    pub completion_tokens: u32,
}

/// A failure from the local inference tier.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The local tier is not currently serving — below the hardware floor, or
    /// unloaded under memory pressure. This is the typed signal the router keys
    /// on to bypass the local tier and proceed remote-only (BR-8).
    #[error("local tier unavailable: {reason}")]
    Unavailable {
        /// User-facing explanation.
        reason: String,
    },
    /// The underlying inference backend failed. The message never contains
    /// prompt content, so it is safe to log.
    #[error("inference backend error: {0}")]
    Backend(String),
}

impl EngineError {
    /// Construct an [`EngineError::Unavailable`] with the given reason.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

/// A local inference backend.
///
/// Bound `Send` so the daemon can hold the engine behind a `Mutex` and share it
/// across client sessions (the one-daemon-per-machine rule, BR-4). Streaming is
/// modelled with an `on_token` callback rather than an async stream to keep this
/// crate runtime-agnostic; the daemon adapts it to its event bus.
pub trait Engine: Send {
    /// The id of the currently loaded model.
    fn model_id(&self) -> &str;

    /// Generate a completion for `prompt`, invoking `on_token` for each emitted
    /// token as it is produced (so callers can measure first-token latency and
    /// stream output).
    ///
    /// # Errors
    /// Returns [`EngineError::Unavailable`] when the local tier is not serving,
    /// or [`EngineError::Backend`] on an inference failure.
    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, EngineError>;
}

/// Availability state of a [`MockEngine`].
#[derive(Debug, Clone)]
enum Availability {
    Available,
    Unavailable(String),
}

/// A deterministic in-memory [`Engine`] for tests and offline development.
///
/// It performs no real inference: it streams a canned, prompt-derived response
/// so higher layers (benchmark, pressure, the daemon) can be exercised without
/// weights. It can also be constructed in an unavailable state to drive the
/// "local tier unavailable" path.
#[derive(Debug, Clone)]
pub struct MockEngine {
    model_id: String,
    availability: Availability,
    canned: Option<String>,
}

impl MockEngine {
    /// A ready mock serving `model_id`.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            availability: Availability::Available,
            canned: None,
        }
    }

    /// A ready mock that always returns `response`, regardless of the prompt.
    pub fn with_response(model_id: impl Into<String>, response: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            availability: Availability::Available,
            canned: Some(response.into()),
        }
    }

    /// A mock whose [`Engine::complete`] always fails with
    /// [`EngineError::Unavailable`] — models an unloaded local tier.
    pub fn unavailable(model_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            availability: Availability::Unavailable(reason.into()),
            canned: None,
        }
    }

    /// The deterministic response for `prompt`.
    fn response_for(&self, prompt: &str) -> String {
        if let Some(canned) = &self.canned {
            return canned.clone();
        }
        let words = prompt.split_whitespace().count();
        format!(
            "label: io ; summary: noted {words} tokens of context via {}",
            self.model_id
        )
    }
}

impl Engine for MockEngine {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, EngineError> {
        if let Availability::Unavailable(reason) = &self.availability {
            return Err(EngineError::Unavailable {
                reason: reason.clone(),
            });
        }

        let text = self.response_for(prompt);
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

// ---------------------------------------------------------------------------
// Real llama.cpp backend — compiled ONLY under `--features llama`.
// ---------------------------------------------------------------------------
//
// This module is excluded from default builds and CI, so llama.cpp (and its
// cmake build) is never compiled there. It is exercised by the `#[ignore]`d,
// feature-gated smoke test in `tests/llama_smoke.rs`, which needs a real GGUF on
// disk. The API here targets `llama-cpp-2` 0.1.x; because it cannot be compiled
// in the default/CI toolchain it is intentionally minimal.
#[cfg(feature = "llama")]
mod llama {
    use super::{Completion, Engine, EngineError, GenParams};
    use std::path::Path;

    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaModel, Special};
    use llama_cpp_2::sampling::LlamaSampler;

    /// A llama.cpp-backed [`Engine`]. Metal is used automatically on Apple
    /// Silicon by offloading all layers to the GPU.
    pub struct LlamaEngine {
        model_id: String,
        backend: LlamaBackend,
        model: LlamaModel,
        n_ctx: u32,
    }

    impl LlamaEngine {
        /// Load a GGUF model from `path`. `gpu_layers` is the number of layers to
        /// offload to the GPU (`u32::MAX` offloads all — the Metal fast path on
        /// Apple Silicon; `0` runs CPU-only).
        ///
        /// # Errors
        /// Returns [`EngineError::Backend`] if the backend or model fails to load.
        pub fn load(
            model_id: impl Into<String>,
            path: &Path,
            gpu_layers: u32,
            n_ctx: u32,
        ) -> Result<Self, EngineError> {
            let backend = LlamaBackend::init().map_err(|e| EngineError::Backend(e.to_string()))?;
            let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
            let model = LlamaModel::load_from_file(&backend, path, &model_params)
                .map_err(|e| EngineError::Backend(e.to_string()))?;
            Ok(Self {
                model_id: model_id.into(),
                backend,
                model,
                n_ctx,
            })
        }
    }

    impl Engine for LlamaEngine {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn complete(
            &self,
            prompt: &str,
            params: &GenParams,
            on_token: &mut dyn FnMut(&str),
        ) -> Result<Completion, EngineError> {
            let ctx_params =
                LlamaContextParams::default().with_n_ctx(std::num::NonZeroU32::new(self.n_ctx));
            let mut ctx = self
                .model
                .new_context(&self.backend, ctx_params)
                .map_err(|e| EngineError::Backend(e.to_string()))?;

            let tokens = self
                .model
                .str_to_token(prompt, AddBos::Always)
                .map_err(|e| EngineError::Backend(e.to_string()))?;
            let prompt_tokens = u32::try_from(tokens.len()).unwrap_or(u32::MAX);

            let mut batch = LlamaBatch::new(self.n_ctx as usize, 1);
            let last = tokens.len().saturating_sub(1);
            for (i, token) in tokens.iter().enumerate() {
                batch
                    .add(*token, i as i32, &[0], i == last)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
            }
            ctx.decode(&mut batch)
                .map_err(|e| EngineError::Backend(e.to_string()))?;

            let mut sampler = LlamaSampler::greedy();
            let mut text = String::new();
            let mut completion_tokens = 0u32;
            let mut n_cur = i32::try_from(tokens.len()).unwrap_or(i32::MAX);

            while completion_tokens < params.max_tokens {
                let token = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(token);
                if self.model.is_eog_token(token) {
                    break;
                }
                let piece = self
                    .model
                    .token_to_str(token, Special::Tokenize)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
                on_token(&piece);
                text.push_str(&piece);
                completion_tokens += 1;

                batch.clear();
                batch
                    .add(token, n_cur, &[0], true)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
                n_cur += 1;
                ctx.decode(&mut batch)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
            }

            Ok(Completion {
                text,
                prompt_tokens,
                completion_tokens,
            })
        }
    }
}

#[cfg(feature = "llama")]
pub use llama::LlamaEngine;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_streams_tokens_and_counts_them() {
        let engine = MockEngine::new("mock-3b");
        let mut streamed = String::new();
        let completion = engine
            .complete("hello there world", &GenParams::default(), &mut |t| {
                streamed.push_str(t);
            })
            .expect("mock completes");
        assert_eq!(engine.model_id(), "mock-3b");
        assert!(completion.completion_tokens > 0);
        assert_eq!(streamed, completion.text);
        // Prompt has three whitespace-delimited words.
        assert_eq!(completion.prompt_tokens, 3);
    }

    #[test]
    fn mock_is_deterministic() {
        let engine = MockEngine::new("mock-3b");
        let a = engine
            .complete("same prompt", &GenParams::default(), &mut |_| {})
            .unwrap();
        let b = engine
            .complete("same prompt", &GenParams::default(), &mut |_| {})
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn max_tokens_caps_the_stream() {
        let engine = MockEngine::with_response("mock", "one two three four five six seven");
        let params = GenParams {
            max_tokens: 3,
            temperature: 0.0,
        };
        let mut count = 0;
        let completion = engine.complete("x", &params, &mut |_| count += 1).unwrap();
        assert_eq!(count, 3);
        assert_eq!(completion.completion_tokens, 3);
    }

    #[test]
    fn unavailable_mock_returns_the_typed_error() {
        let engine = MockEngine::unavailable("mock-3b", "unloaded under memory pressure");
        let err = engine
            .complete("anything", &GenParams::default(), &mut |_| {})
            .unwrap_err();
        match err {
            EngineError::Unavailable { reason } => {
                assert!(reason.contains("memory pressure"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        // The Display form is the user-facing "local tier unavailable" string.
        assert!(engine
            .complete("x", &GenParams::default(), &mut |_| {})
            .unwrap_err()
            .to_string()
            .starts_with("local tier unavailable"));
    }
}
