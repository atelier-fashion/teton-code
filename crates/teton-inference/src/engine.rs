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
    use std::sync::OnceLock;

    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;
    use llama_cpp_2::token::LlamaToken;
    use llama_cpp_2::TokenToStringError;

    /// The process-wide llama.cpp backend.
    ///
    /// `LlamaBackend::init` is once-per-process by construction (a global
    /// `compare_exchange` that errors on the second call), so an engine cannot
    /// own its backend: the first model switch of a daemon's lifetime would
    /// find the flag already set and report perfectly good weights as
    /// unloadable. One initialization, shared by every engine this process
    /// ever loads, held in a static so it is never freed while any engine
    /// could still be using it.
    fn shared_backend() -> Result<&'static LlamaBackend, EngineError> {
        static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
        BACKEND
            .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|reason| EngineError::Backend(reason.clone()))
    }

    /// The logical batch ceiling for one `decode` call, in tokens.
    ///
    /// Passed to the context as `n_batch` and used to chunk prompt decoding, so
    /// the two can never disagree — llama.cpp enforces `n_tokens <= n_batch`
    /// with a process-aborting `GGML_ASSERT`, not a returnable error. Matches
    /// llama.cpp's own default logical batch size.
    const N_BATCH: u32 = 2048;

    /// Streaming token-piece decoder: raw llama.cpp piece bytes in, valid
    /// UTF-8 out.
    ///
    /// ONE decoder lives for the whole generation stream, because a single BPE
    /// token can end midway through a multi-byte UTF-8 character — the decoder
    /// is what carries the partial bytes across to the next token. The
    /// deprecated per-token `token_to_str` this replaced constructed a fresh
    /// decoder per call and dropped it, losing any held partial sequence and
    /// garbling streamed CJK/emoji at token boundaries.
    ///
    /// The bytes are decoded here rather than through the non-deprecated
    /// `LlamaModel::token_to_piece` wrapper because that wrapper under-reserves
    /// its output buffer (`bytes.len()`, while a *completing* multi-byte
    /// character emits up to 3 more bytes than this token carried, and each
    /// malformed byte becomes a 3-byte U+FFFD) — and `decode_to_string` writes
    /// only into spare capacity, so on overflow the wrapper silently discards
    /// the undecoded input. [`encoding_rs::Decoder::max_utf8_buffer_length`]
    /// is the contract for "the whole input will be consumed", so this type
    /// reserves that instead.
    struct PieceDecoder {
        decoder: encoding_rs::Decoder,
    }

    impl PieceDecoder {
        fn new() -> Self {
            Self {
                decoder: encoding_rs::UTF_8.new_decoder(),
            }
        }

        /// Decode one token's piece bytes, returning whatever printable text
        /// they complete. Empty when the token only *starts* a multi-byte
        /// character — its bytes are held for the next call. Malformed bytes
        /// become U+FFFD; they are never an error and never dropped.
        fn push(&mut self, bytes: &[u8]) -> String {
            let mut out = String::with_capacity(
                self.decoder
                    .max_utf8_buffer_length(bytes.len())
                    .unwrap_or(bytes.len().saturating_mul(3) + 16),
            );
            let (_result, read, _replaced) = self.decoder.decode_to_string(bytes, &mut out, false);
            debug_assert_eq!(
                read,
                bytes.len(),
                "a max_utf8_buffer_length'd decode consumes all input"
            );
            out
        }

        /// End the stream: an incomplete sequence still held becomes U+FFFD
        /// rather than silently vanishing.
        fn finish(mut self) -> String {
            let mut out =
                String::with_capacity(self.decoder.max_utf8_buffer_length(0).unwrap_or(16));
            let _ = self.decoder.decode_to_string(&[], &mut out, true);
            out
        }
    }

    /// One sampled token's raw piece bytes, with special tokens rendered as
    /// their text (the behavior the old `Special::Tokenize` selected).
    ///
    /// Mirrors the upstream wrapper's retry contract: an 8-byte first guess,
    /// then the exact size llama.cpp reports back when that was short. A
    /// nonsensical reported size falls through to the second call's own typed
    /// error rather than panicking on a worker thread.
    fn piece_bytes(model: &LlamaModel, token: LlamaToken) -> Result<Vec<u8>, EngineError> {
        match model.token_to_piece_bytes(token, 8, true, None) {
            Err(TokenToStringError::InsufficientBufferSpace(need)) => model.token_to_piece_bytes(
                token,
                usize::try_from(need.unsigned_abs()).unwrap_or(0),
                true,
                None,
            ),
            got => got,
        }
        .map_err(|e| EngineError::Backend(e.to_string()))
    }

    /// A llama.cpp-backed [`Engine`]. Metal is used automatically on Apple
    /// Silicon by offloading all layers to the GPU.
    pub struct LlamaEngine {
        model_id: String,
        backend: &'static LlamaBackend,
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
            let backend = shared_backend()?;
            let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
            let model = LlamaModel::load_from_file(backend, path, &model_params)
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
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(std::num::NonZeroU32::new(self.n_ctx))
                .with_n_batch(N_BATCH);
            let mut ctx = self
                .model
                .new_context(self.backend, ctx_params)
                .map_err(|e| EngineError::Backend(e.to_string()))?;

            let tokens = self
                .model
                .str_to_token(prompt, AddBos::Always)
                .map_err(|e| EngineError::Backend(e.to_string()))?;
            let prompt_tokens = u32::try_from(tokens.len()).unwrap_or(u32::MAX);

            // Refuse an over-window prompt with a typed error BEFORE any llama.cpp
            // call sees it. llama.cpp enforces its limits with GGML_ASSERT — an
            // `abort()`, not a catchable error — so feeding it an input that
            // cannot fit would take down the whole daemon process, as the first
            // dogfooded over-window turn did.
            let budget = self.n_ctx.saturating_sub(params.max_tokens);
            if prompt_tokens > budget {
                return Err(EngineError::Backend(format!(
                    "prompt of {prompt_tokens} tokens exceeds this engine's window \
                     ({budget} = {} context minus {} generation)",
                    self.n_ctx, params.max_tokens
                )));
            }

            // Decode the prompt in `n_batch`-sized chunks: one `decode` may not
            // exceed the context's logical batch size (GGML_ASSERT, as above).
            // Only the final token of the final chunk requests logits — that is
            // where generation starts.
            let mut batch = LlamaBatch::new(N_BATCH as usize, 1);
            let last = tokens.len().saturating_sub(1);
            let mut pos = 0usize;
            for chunk in tokens.chunks(N_BATCH as usize) {
                batch.clear();
                for (i, token) in chunk.iter().enumerate() {
                    let index = pos + i;
                    batch
                        .add(*token, index as i32, &[0], index == last)
                        .map_err(|e| EngineError::Backend(e.to_string()))?;
                }
                ctx.decode(&mut batch)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
                pos += chunk.len();
            }

            let mut sampler = LlamaSampler::greedy();
            let mut text = String::new();
            let mut completion_tokens = 0u32;
            let mut n_cur = i32::try_from(tokens.len()).unwrap_or(i32::MAX);
            // One [`PieceDecoder`] for the whole stream — see its docs for why
            // the decoder must outlive every token.
            let mut decoder = PieceDecoder::new();

            while completion_tokens < params.max_tokens {
                let token = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(token);
                if self.model.is_eog_token(token) {
                    break;
                }
                let piece = decoder.push(&piece_bytes(&self.model, token)?);
                // A token that only *starts* a multi-byte character yields an
                // empty piece (its bytes are held in the decoder) — nothing to
                // stream yet, but it still counts as a generated token.
                if !piece.is_empty() {
                    on_token(&piece);
                    text.push_str(&piece);
                }
                completion_tokens += 1;

                batch.clear();
                batch
                    .add(token, n_cur, &[0], true)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
                n_cur += 1;
                ctx.decode(&mut batch)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
            }

            // The stream can end (EOG or max_tokens) while the decoder holds an
            // incomplete sequence; the flush turns it into U+FFFD rather than
            // dropping the bytes silently.
            let tail = decoder.finish();
            if !tail.is_empty() {
                on_token(&tail);
                text.push_str(&tail);
            }

            Ok(Completion {
                text,
                prompt_tokens,
                completion_tokens,
            })
        }
    }

    /// [`PieceDecoder`] needs no model, so its contract — the one the old
    /// per-token `token_to_str` broke — is pinned here and runs under any
    /// `--features llama` test invocation.
    #[cfg(test)]
    mod tests {
        use super::PieceDecoder;

        /// The regression the migration exists for: a multi-byte character
        /// split across token pieces must come out whole, which requires the
        /// decoder to survive between pieces.
        #[test]
        fn a_character_split_across_pieces_is_reassembled() {
            let mut decoder = PieceDecoder::new();
            // "日" (E6 97 A5) one byte per piece; "🦀" (F0 9F A6 80) split 2+2.
            assert_eq!(decoder.push(&[0xE6]), "");
            assert_eq!(decoder.push(&[0x97]), "");
            assert_eq!(decoder.push(&[0xA5]), "日");
            assert_eq!(decoder.push(&[0xF0, 0x9F]), "");
            assert_eq!(decoder.push(&[0xA6, 0x80]), "🦀");
            assert_eq!(decoder.finish(), "", "a clean stream has nothing to flush");
        }

        /// A completing multi-byte character emits MORE bytes than the final
        /// piece carried — the exact case the upstream `token_to_piece`
        /// wrapper's `with_capacity(bytes.len())` under-reserves for and then
        /// silently drops.
        #[test]
        fn a_completing_character_larger_than_its_final_piece_is_not_dropped() {
            let mut decoder = PieceDecoder::new();
            assert_eq!(decoder.push(&[0xF0, 0x9F, 0xA6]), "");
            // One input byte, four output bytes.
            assert_eq!(decoder.push(&[0x80]), "🦀");
        }

        /// A stream that ends mid-character flushes U+FFFD — the bytes are
        /// accounted for, never silently vanished.
        #[test]
        fn a_truncated_stream_flushes_a_replacement_character() {
            let mut decoder = PieceDecoder::new();
            assert_eq!(decoder.push(&[0xE6, 0x97]), "");
            assert_eq!(decoder.finish(), "\u{FFFD}");
        }

        /// Malformed bytes become U+FFFD inline and the stream keeps going —
        /// never an error, never dropped.
        #[test]
        fn malformed_bytes_are_replaced_and_the_stream_continues() {
            let mut decoder = PieceDecoder::new();
            assert_eq!(decoder.push(&[0xFF, b'o', b'k']), "\u{FFFD}ok");
            assert_eq!(decoder.push("日".as_bytes()), "日");
            assert_eq!(decoder.finish(), "");
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
