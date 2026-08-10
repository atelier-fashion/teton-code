//! The [`Engine`] abstraction over local inference.
//!
//! Everything above this trait — probe, download, benchmark, pressure — is
//! backend-agnostic and tests against [`MockEngine`]. The real llama.cpp binding
//! lives in [`LlamaEngine`], compiled only under the non-default `llama` feature
//! so that default builds and CI never pull in llama.cpp or cmake (see the crate
//! docs). The daemon selects the backend at runtime.

use crate::prefix_cache::MissReason;

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
    /// Prompt tokens whose KV was reused from the resident prefix (REQ-564
    /// BR-9). Always `0` on the cold path, which is every path except
    /// [`Engine::complete_cached`] against a cache-bearing engine.
    ///
    /// The *processed* count is deliberately not stored: it is
    /// `prompt_tokens - cached_tokens` and nothing else. Two stored counts that
    /// must sum to a third is a drift surface (LESSON-446) — derive it with
    /// [`Completion::processed_tokens`].
    pub cached_tokens: u32,
    /// Why the prefix cache did not serve this completion, or `None` on a hit.
    ///
    /// A cold path reports [`MissReason::Cold`]; `None` means "reused", never
    /// "unknown". An engine *error* is never expressed here — it is an `Err`
    /// (BR-8).
    pub cache_miss: Option<MissReason>,
}

impl Completion {
    /// A cold completion of `text`: nothing reused, miss reason `Cold`.
    ///
    /// The single constructor every non-caching engine uses, so the two new
    /// REQ-564 fields cannot drift apart across implementors.
    #[must_use]
    pub fn cold(text: String, prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            text,
            prompt_tokens,
            completion_tokens,
            cached_tokens: 0,
            cache_miss: Some(MissReason::Cold),
        }
    }

    /// Prompt tokens this completion actually had to prefill.
    #[must_use]
    pub fn processed_tokens(&self) -> u32 {
        self.prompt_tokens.saturating_sub(self.cached_tokens)
    }
}

/// The typed refusal for a prompt that cannot fit the engine's window, or
/// `None` when it fits.
///
/// **One expression, every path** (BR-7, LESSON-491). llama.cpp enforces its
/// limits with `GGML_ASSERT` — an `abort()`, not a catchable error — so an
/// over-window prompt would take down the whole daemon process, as the first
/// dogfooded over-window turn did (LESSON-444). Prefix reuse changes how many
/// tokens must be *decoded*; it does not change how many must *fit*, because
/// the KV still has to hold the entire prompt. So the guard measures the full
/// tokenized prompt and runs ahead of the cache probe, and the cached and cold
/// paths are guarded by this one function rather than by two copies that can
/// drift.
///
/// Free-standing and feature-free on purpose: the scripted test engines call
/// the same function the real engine does, so the acceptance suite cannot pass
/// against a laxer guard than production runs (AC-8).
#[must_use]
pub fn over_window(prompt_tokens: u32, n_ctx: u32, max_tokens: u32) -> Option<EngineError> {
    let budget = n_ctx.saturating_sub(max_tokens);
    if prompt_tokens > budget {
        return Some(EngineError::Backend(format!(
            "prompt of {prompt_tokens} tokens exceeds this engine's window \
             ({budget} = {n_ctx} context minus {max_tokens} generation)"
        )));
    }
    None
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

/// The prompt-rendering family an engine wants.
///
/// Carried as engine metadata and resolved from the loaded model's GGUF chat
/// template (REQ-554 ADR-2): the harness renders each turn in this family and
/// selects its fabrication markers to match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFormat {
    /// The legacy flat `User:`/`Assistant:` transcript rendering, and the
    /// universal fallback: a model whose template is absent or unrecognized
    /// serves on it with today's behavior preserved exactly (REQ-554 BR-2).
    /// Every test double and scripted engine stays here by trait default.
    Flat,
    /// ChatML — `<|im_start|>role\n…<|im_end|>\n`. The family the entire
    /// current catalog ships.
    ChatMl,
}

/// The rendering family implied by a GGUF `tokenizer.chat_template` string.
///
/// A template is [`ChatFormat::ChatMl`] only when it carries **both** ChatML
/// delimiters (`<|im_start|>` and `<|im_end|>`) and none of the dialect
/// separators this renderer cannot reproduce (`<|im_sep|>`, the Phi-4 family —
/// which *contains* `<|im_start|>` but expects a separator where our renderer
/// writes a newline). Everything else — an empty string, absent metadata, an
/// unrecognized or merely-mentioning template — is [`ChatFormat::Flat`], the
/// fallback (REQ-554 BR-2). Misclassifying toward `ChatMl` is the dangerous
/// direction: it renders a format the model was not trained on with no further
/// fallback, so detection is corroborated rather than a single substring
/// (REQ-554 verify). Recognition is delimiter matching, not Jinja evaluation.
///
/// Detection is deliberately a pure function over the template *string* rather
/// than an FFI probe, so the matcher is pinned by the default/CI suite with no
/// `llama` feature and no weights on disk (REQ-554 ADR-1/AC-8). The
/// feature-gated `LlamaEngine` is only its caller. The template string is
/// third-party bytes (ADR-005 trusts the quantizer), so the matcher must be
/// total and cheap on adversarial input — `contains` on three fixed needles
/// is O(n), allocation-free, and cannot panic.
#[must_use]
pub fn detect_chat_format(template: &str) -> ChatFormat {
    let chatml = template.contains("<|im_start|>")
        && template.contains("<|im_end|>")
        && !template.contains("<|im_sep|>");
    if chatml {
        ChatFormat::ChatMl
    } else {
        ChatFormat::Flat
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
    /// `on_token` returns whether generation should **continue**: `false` stops
    /// the completion early, and the returned [`Completion::text`] contains only
    /// what was emitted up to the stop. This is how the harness ends a weak
    /// model's turn at its first tool call instead of letting it run on and
    /// fabricate the rest of the transcript (BUG-147).
    ///
    /// # Errors
    /// Returns [`EngineError::Unavailable`] when the local tier is not serving,
    /// or [`EngineError::Backend`] on an inference failure.
    fn complete(
        &self,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError>;

    /// Generate a completion that may reuse `session`'s resident KV prefix
    /// (REQ-564).
    ///
    /// This is the **agent-turn** entry point, and the only one that carries a
    /// cache key. Duty calls (`summarize`, `classify`, `triage`, `shell`,
    /// `compact`, redaction) keep calling [`Engine::complete`], which is why
    /// BR-5 holds structurally rather than by discipline: a duty has no way to
    /// name a session, so it cannot evict the agent's prefix even by mistake.
    /// OQ-1 resolved this as cold-per-duty.
    ///
    /// Defaulted to a cold delegation, so every engine that does not cache —
    /// [`MockEngine`], the scripted and gated test doubles — keeps working
    /// unchanged and honestly reports [`MissReason::Cold`].
    ///
    /// # Errors
    /// As [`Engine::complete`]. A cache miss is **not** an error: it is a
    /// successful completion whose `cache_miss` field names the reason (BR-8).
    fn complete_cached(
        &mut self,
        _session: &str,
        prompt: &str,
        params: &GenParams,
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        self.complete(prompt, params, on_token)
    }

    /// Drop any resident KV prefix (REQ-564 BR-4).
    ///
    /// Must never fail a subsequent turn — the next completion cold-prefills
    /// and reports [`MissReason::Evicted`] so the drop is visible rather than
    /// silent. Defaulted to a no-op for engines that hold no cache.
    fn evict_prefix_cache(&mut self, _reason: crate::prefix_cache::EvictionReason) {}

    /// The prompt-rendering family this engine expects.
    ///
    /// Resolved once for a loaded engine and immutable thereafter; the harness
    /// renders every prompt it hands to [`Engine::complete`] in this family and
    /// selects its fabrication markers from it (REQ-554 ADR-2/ADR-4).
    ///
    /// Defaulted to [`ChatFormat::Flat`] so every existing implementor —
    /// scripted, gated, and mock engines — keeps serving the flat transcript
    /// without a single edit.
    fn chat_format(&self) -> ChatFormat {
        ChatFormat::Flat
    }
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
    chat_format: ChatFormat,
}

impl MockEngine {
    /// The shared base every public constructor builds on: available, no canned
    /// response, flat rendering. One site owns the defaults so the constructors
    /// cannot drift apart.
    fn base(model_id: String) -> Self {
        Self {
            model_id,
            availability: Availability::Available,
            canned: None,
            chat_format: ChatFormat::Flat,
        }
    }

    /// A ready mock serving `model_id`.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self::base(model_id.into())
    }

    /// A ready mock that always returns `response`, regardless of the prompt.
    pub fn with_response(model_id: impl Into<String>, response: impl Into<String>) -> Self {
        Self {
            canned: Some(response.into()),
            ..Self::base(model_id.into())
        }
    }

    /// A mock whose [`Engine::complete`] always fails with
    /// [`EngineError::Unavailable`] — models an unloaded local tier.
    pub fn unavailable(model_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            availability: Availability::Unavailable(reason.into()),
            ..Self::base(model_id.into())
        }
    }

    /// This mock, reporting `format` from [`Engine::chat_format`].
    ///
    /// The mock performs no real inference and ignores the format when
    /// generating; it exists so wiring tests can simulate a template-bearing
    /// engine without the `llama` feature or weights on disk (REQ-554 AC-8).
    #[must_use]
    pub fn with_chat_format(mut self, format: ChatFormat) -> Self {
        self.chat_format = format;
        self
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
        on_token: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Completion, EngineError> {
        if let Availability::Unavailable(reason) = &self.availability {
            return Err(EngineError::Unavailable {
                reason: reason.clone(),
            });
        }

        let full = self.response_for(prompt);
        // Text reflects what was actually emitted: an early stop (caller
        // returned `false`) or the max_tokens cap truncates it, matching the
        // real backend's contract.
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

    fn chat_format(&self) -> ChatFormat {
        self.chat_format
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
    use super::{detect_chat_format, ChatFormat, Completion, Engine, EngineError, GenParams};
    use crate::prefix_cache::{CacheDecision, EvictionReason, MissReason, PrefixCacheState};
    use std::path::Path;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::OnceLock;
    use std::thread::JoinHandle;

    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::context::LlamaContext;
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
    /// One message from the worker thread to the caller of a completion.
    enum Emission {
        /// A decoded piece of text. The worker blocks until the caller answers
        /// on the control channel with whether to keep going.
        Token(String),
        /// The completion finished (or failed). Always the last message.
        Done(Box<Result<Completion, EngineError>>),
    }

    /// A unit of work for the model-owning thread.
    enum Request {
        Complete {
            rendered: String,
            params: GenParams,
            /// `Some(session)` may reuse the resident prefix; `None` is a cold
            /// call on its own throwaway context (every duty — BR-5).
            cache_key: Option<String>,
            out_tx: Sender<Emission>,
            ctrl_rx: Receiver<bool>,
        },
        Evict,
    }

    /// What the worker learned about the model at load time.
    struct LoadedMeta {
        chat_format: ChatFormat,
        template_fallback_reason: Option<&'static str>,
    }

    /// A llama.cpp-backed [`Engine`]. Metal is used automatically on Apple
    /// Silicon by offloading all layers to the GPU.
    ///
    /// # Why this is a thread handle and not a struct holding the model
    ///
    /// REQ-564 keeps one `LlamaContext` alive across turns so a turn that
    /// extends the previous prompt prefills only the new suffix. Two properties
    /// of the binding make the obvious "add a `cache: Option<LlamaContext>`
    /// field" unsound:
    ///
    /// 1. **Self-reference.** `LlamaModel::new_context<'a>(&'a self, …) ->
    ///    LlamaContext<'a>` ties the context to a borrow of the model, so a
    ///    struct holding both is self-referential — it needs `unsafe` lifetime
    ///    erasure or a crate like `ouroboros`.
    /// 2. **`LlamaContext` is `!Send`.** It holds a raw `NonNull<llama_context>`
    ///    and the binding declares no `unsafe impl Send` (contrast `LlamaModel`,
    ///    which has both). But [`Engine`] is `Send`, the daemon shares the engine
    ///    as `Arc<Mutex<dyn Engine>>`, and successive turns run on *different*
    ///    `spawn_blocking` threads. Holding the context here would force an
    ///    `unsafe impl Send` asserting that llama.cpp contexts — including their
    ///    Metal command queues — have no thread affinity, a claim about a callee
    ///    we cannot discharge from its source (LESSON-453).
    ///
    /// So the model and its context live together on one owned thread, where the
    /// borrow is an ordinary stack borrow and the context never crosses a thread
    /// boundary. Both problems disappear and this module contains no `unsafe`.
    pub struct LlamaEngine {
        model_id: String,
        chat_format: ChatFormat,
        /// Why this engine fell back to [`ChatFormat::Flat`], when it did —
        /// carried for the loader's user-visible downgrade report (REQ-554
        /// BR-2, LESSON-456: never discard the reason a degradation happened).
        /// `None` when a template was recognized.
        template_fallback_reason: Option<&'static str>,
        /// To the model-owning thread. Dropping it ends that thread's loop,
        /// which is how [`Drop`] shuts the worker down without a sentinel
        /// message that could be missed.
        tx: Option<Sender<Request>>,
        worker: Option<JoinHandle<()>>,
    }

    impl LlamaEngine {
        /// Load a GGUF model from `path`. `gpu_layers` is the number of layers to
        /// offload to the GPU (`u32::MAX` offloads all — the Metal fast path on
        /// Apple Silicon; `0` runs CPU-only).
        ///
        /// The model is loaded **on** the worker thread, and this call blocks
        /// until that load reports back — externally identical to the previous
        /// synchronous load, which also blocked its caller for minutes.
        ///
        /// The GGUF's `tokenizer.chat_template` metadata is read there, once, and
        /// reduced to a [`ChatFormat`] by the pure matcher (REQ-554 ADR-1/ADR-2).
        /// Every way that read can fail — no template metadata
        /// (`ChatTemplateError::MissingTemplate`), an interior NUL, non-UTF-8
        /// bytes, an unrecognized family — resolves to [`ChatFormat::Flat`], the
        /// fallback rendering. A model that cannot describe its template still
        /// loads and still serves (REQ-554 BR-2/BR-6).
        ///
        /// # Errors
        /// Returns [`EngineError::Backend`] if the backend or model fails to load.
        pub fn load(
            model_id: impl Into<String>,
            path: &Path,
            gpu_layers: u32,
            n_ctx: u32,
        ) -> Result<Self, EngineError> {
            let model_id = model_id.into();
            let owned_path = path.to_path_buf();
            let (tx, rx) = mpsc::channel::<Request>();
            let (load_tx, load_rx) = mpsc::channel::<Result<LoadedMeta, EngineError>>();

            let worker = std::thread::Builder::new()
                // Named so a stuck inference is identifiable in a sample/backtrace
                // rather than being one anonymous thread among the pool's.
                .name(format!("teton-llama-{model_id}"))
                .spawn(move || worker_main(&owned_path, gpu_layers, n_ctx, &load_tx, &rx))
                .map_err(|e| {
                    EngineError::Backend(format!("could not start the inference thread: {e}"))
                })?;

            // A worker that died before replying drops `load_tx`, so this recv
            // fails rather than hanging — the error is never silent.
            let meta = match load_rx.recv() {
                Ok(result) => result?,
                Err(_) => {
                    let _ = worker.join();
                    return Err(EngineError::Backend(
                        "the inference thread stopped before the model finished loading".to_owned(),
                    ));
                }
            };

            Ok(Self {
                model_id,
                chat_format: meta.chat_format,
                template_fallback_reason: meta.template_fallback_reason,
                tx: Some(tx),
                worker: Some(worker),
            })
        }

        /// Why this engine is on the flat fallback, when it is (REQ-554 BR-2).
        /// The loader interpolates this into the downgrade report it emits at
        /// commit time; `None` means a template was recognized.
        #[must_use]
        pub fn template_fallback_reason(&self) -> Option<&'static str> {
            self.template_fallback_reason
        }

        /// Drive one completion on the worker thread, bridging its token stream
        /// back through `on_token`.
        ///
        /// The control channel is created here and dropped when this returns, so
        /// it is exactly as long-lived as the call: the worker can never block on
        /// an answer from a caller that has gone away.
        fn run(
            &self,
            cache_key: Option<String>,
            prompt: &str,
            params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            let stopped = || {
                EngineError::Backend("the local inference thread is no longer running".to_owned())
            };
            let tx = self.tx.as_ref().ok_or_else(stopped)?;

            let (out_tx, out_rx) = mpsc::channel::<Emission>();
            let (ctrl_tx, ctrl_rx) = mpsc::channel::<bool>();
            tx.send(Request::Complete {
                rendered: prompt.to_owned(),
                params: *params,
                cache_key,
                out_tx,
                ctrl_rx,
            })
            .map_err(|_| stopped())?;

            loop {
                match out_rx.recv() {
                    Ok(Emission::Token(piece)) => {
                        let keep_going = on_token(&piece);
                        // A send failure means the worker already finished (it
                        // stops reading control after the final piece); the
                        // `Done` message is still queued, so keep draining.
                        let _ = ctrl_tx.send(keep_going);
                    }
                    Ok(Emission::Done(result)) => return *result,
                    // The worker panicked or vanished without a `Done`. A
                    // panicked worker is a backend failure, never a daemon crash.
                    Err(_) => return Err(stopped()),
                }
            }
        }
    }

    impl Drop for LlamaEngine {
        fn drop(&mut self) {
            // Dropping the sender ends the worker's `recv` loop; the join then
            // waits for the model (and any resident context) to be freed before
            // this engine is considered gone. Without the join, an engine swap
            // could hold two models' weights resident at once.
            self.tx = None;
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl Engine for LlamaEngine {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn chat_format(&self) -> ChatFormat {
            self.chat_format
        }

        fn complete(
            &self,
            prompt: &str,
            params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            // No cache key: a throwaway context, prefilled from zero, dropped at
            // the end. This is every duty call (OQ-1's cold-per-duty), and it
            // leaves the agent session's resident prefix untouched (BR-5).
            self.run(None, prompt, params, on_token)
        }

        fn complete_cached(
            &mut self,
            session: &str,
            prompt: &str,
            params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            self.run(Some(session.to_owned()), prompt, params, on_token)
        }

        fn evict_prefix_cache(&mut self, _reason: EvictionReason) {
            // Best-effort by contract: a dropped cache must never fail a turn
            // (BR-4), so a worker that has already gone away is not an error
            // here — the next turn cold-prefills, which is exactly the intended
            // post-eviction behavior.
            if let Some(tx) = self.tx.as_ref() {
                let _ = tx.send(Request::Evict);
            }
        }
    }

    /// The model-owning thread.
    ///
    /// `model` and `resident` are both locals here, and `resident` is declared
    /// after `model`, so Rust's reverse-declaration drop order frees the context
    /// before the model it borrows. That ordering is the whole safety argument
    /// for keeping them together, and it is enforced by the compiler rather than
    /// by a comment.
    fn worker_main(
        path: &Path,
        gpu_layers: u32,
        n_ctx: u32,
        load_tx: &Sender<Result<LoadedMeta, EngineError>>,
        rx: &Receiver<Request>,
    ) {
        let backend = match shared_backend() {
            Ok(backend) => backend,
            Err(e) => {
                let _ = load_tx.send(Err(e));
                return;
            }
        };
        let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
        let model = match LlamaModel::load_from_file(backend, path, &model_params) {
            Ok(model) => model,
            Err(e) => {
                let _ = load_tx.send(Err(EngineError::Backend(e.to_string())));
                return;
            }
        };

        // Every failure mode resolves to Flat, but the CAUSE is kept, not
        // discarded (LESSON-456): the loader interpolates it into the
        // user-visible fallback report, so "no template at all" and "a
        // template we can't reproduce" read differently.
        let (chat_format, template_fallback_reason) = match model.chat_template(None) {
            Err(_) => (
                ChatFormat::Flat,
                Some("no chat template in the GGUF metadata"),
            ),
            Ok(template) => match template.to_str() {
                Err(_) => (
                    ChatFormat::Flat,
                    Some("the GGUF chat template is not valid UTF-8"),
                ),
                Ok(text) => match detect_chat_format(text) {
                    ChatFormat::ChatMl => (ChatFormat::ChatMl, None),
                    ChatFormat::Flat => {
                        (ChatFormat::Flat, Some("unrecognized chat template family"))
                    }
                },
            },
        };
        if load_tx
            .send(Ok(LoadedMeta {
                chat_format,
                template_fallback_reason,
            }))
            .is_err()
        {
            // The loader gave up while we were loading; nothing to serve.
            return;
        }

        // The single cache slot (BR-3) and the context whose KV it describes.
        // Declared after `model` so they drop before it.
        let mut resident: Option<LlamaContext<'_>> = None;
        let mut cache = PrefixCacheState::new();

        while let Ok(request) = rx.recv() {
            match request {
                Request::Evict => {
                    resident = None;
                    cache.evict();
                }
                Request::Complete {
                    rendered,
                    params,
                    cache_key,
                    out_tx,
                    ctrl_rx,
                } => {
                    let result = serve(
                        backend,
                        &model,
                        n_ctx,
                        &mut resident,
                        &mut cache,
                        cache_key.as_deref(),
                        &rendered,
                        &params,
                        &out_tx,
                        &ctrl_rx,
                    );
                    let _ = out_tx.send(Emission::Done(Box::new(result)));
                }
            }
        }
    }

    /// Serve one completion, reusing the resident prefix when the policy allows.
    #[allow(clippy::too_many_arguments)]
    fn serve<'m>(
        backend: &'static LlamaBackend,
        model: &'m LlamaModel,
        n_ctx: u32,
        resident: &mut Option<LlamaContext<'m>>,
        cache: &mut PrefixCacheState,
        cache_key: Option<&str>,
        prompt: &str,
        params: &GenParams,
        out_tx: &Sender<Emission>,
        ctrl_rx: &Receiver<bool>,
    ) -> Result<Completion, EngineError> {
        // BEHAVIORAL DEPENDENCY (REQ-554 security): llama-cpp-2 0.1.151's
        // `str_to_token` hardcodes `parse_special = true`, so control-token
        // spellings ANYWHERE in this string tokenize as real ChatML control
        // tokens, not text. The harness renderer is the compensating
        // control — it defuses those spellings in untrusted content before
        // they reach this call (`tetond`'s `neutralize_control_tokens`).
        // Re-audit that pairing if this binding is ever bumped: an upstream
        // flip of that flag silently changes the injection posture in
        // either direction.
        let tokens = model
            .str_to_token(prompt, AddBos::Always)
            .map_err(|e| EngineError::Backend(e.to_string()))?;
        let prompt_tokens = u32::try_from(tokens.len()).unwrap_or(u32::MAX);

        // The over-window guard runs HERE — after tokenization, before any
        // llama.cpp call, and before the cache probe — so it covers the reuse
        // path and the cold path with one expression (BR-7, LESSON-491).
        // Reuse changes how many tokens are decoded, never how many must fit:
        // the KV still has to hold the whole prompt.
        if let Some(refusal) = super::over_window(prompt_tokens, n_ctx, params.max_tokens) {
            return Err(refusal);
        }

        let ids: Vec<i32> = tokens.iter().map(|t| t.0).collect();

        // A duty call (no cache key) gets its own throwaway context and never
        // touches the resident one (BR-5).
        let Some(session) = cache_key else {
            let mut ctx = new_context(model, backend, n_ctx)?;
            let generated = run_generation(&mut ctx, model, &tokens, 0, params, out_tx, ctrl_rx)?;
            return Ok(generated.into_completion(prompt_tokens, 0, Some(MissReason::Cold)));
        };

        // The context must exist before the probe can mean anything: a slot
        // whose context was dropped describes nothing.
        if resident.is_none() {
            *resident = Some(new_context(model, backend, n_ctx)?);
            // Losing the context loses the KV it held, so the description must
            // go with it or the next probe compares against a phantom.
            if !cache.is_empty() {
                cache.evict();
            }
        }
        let decision = cache.probe(session, &ids);
        let ctx = resident
            .as_mut()
            .expect("the resident context was just installed");

        let start = match decision {
            CacheDecision::Hit { reuse } => {
                // Rewind the KV to the agreement point. Everything past it is
                // another turn's history and must not survive into this one.
                ctx.clear_kv_cache_seq(Some(0), u32::try_from(reuse).ok(), None)
                    .map_err(|e| EngineError::Backend(e.to_string()))?;
                reuse
            }
            CacheDecision::Miss(_) => {
                ctx.clear_kv_cache();
                0
            }
        };

        let cached_tokens = u32::try_from(start).unwrap_or(u32::MAX);
        let generated = match run_generation(ctx, model, &tokens, start, params, out_tx, ctrl_rx) {
            Ok(generated) => generated,
            Err(e) => {
                // The KV is now in an unknown state — some of the prompt may be
                // decoded, some not. The recorded prefix would no longer describe
                // it, so drop both rather than let the next turn reuse a
                // description we cannot vouch for. A fallback must preserve the
                // invariant it guards (LESSON-447); here that invariant is "the
                // recorded prefix describes the resident KV exactly".
                *resident = None;
                cache.evict();
                return Err(e);
            }
        };

        // Record what the KV actually holds: the prompt PLUS every token decoded
        // during generation. Recording only the prompt would leave the
        // description shorter than the real KV and the next turn's reuse offset
        // would be computed against the wrong baseline — a correctness bug, not
        // a performance one.
        let mut resident_ids = ids;
        resident_ids.extend(generated.tokens.iter().map(|t| t.0));
        cache.record(session, resident_ids);

        Ok(generated.into_completion(prompt_tokens, cached_tokens, decision.miss_reason()))
    }

    /// A fresh context sized to this engine's window.
    fn new_context<'m>(
        model: &'m LlamaModel,
        backend: &'static LlamaBackend,
        n_ctx: u32,
    ) -> Result<LlamaContext<'m>, EngineError> {
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(n_ctx))
            .with_n_batch(N_BATCH);
        model
            .new_context(backend, ctx_params)
            .map_err(|e| EngineError::Backend(e.to_string()))
    }

    /// What one generation produced.
    struct Generated {
        text: String,
        completion_tokens: u32,
        /// The tokens decoded into the context during generation — needed to
        /// describe the resident KV afterwards.
        tokens: Vec<LlamaToken>,
    }

    impl Generated {
        fn into_completion(
            self,
            prompt_tokens: u32,
            cached_tokens: u32,
            cache_miss: Option<MissReason>,
        ) -> Completion {
            Completion {
                text: self.text,
                prompt_tokens,
                completion_tokens: self.completion_tokens,
                cached_tokens,
                cache_miss,
            }
        }
    }

    /// Prefill `tokens[start..]` and then decode until EOG, the cap, or an early
    /// stop from the caller.
    #[allow(clippy::too_many_arguments)]
    fn run_generation(
        ctx: &mut LlamaContext<'_>,
        model: &LlamaModel,
        tokens: &[LlamaToken],
        start: usize,
        params: &GenParams,
        out_tx: &Sender<Emission>,
        ctrl_rx: &Receiver<bool>,
    ) -> Result<Generated, EngineError> {
        // Decode the prompt suffix in `n_batch`-sized chunks: one `decode` may
        // not exceed the context's logical batch size (GGML_ASSERT, which aborts
        // the process rather than returning — LESSON-444). Only the final token
        // of the final chunk requests logits — that is where generation starts.
        //
        // Positions are absolute, so a reused prefix of `start` tokens means the
        // suffix occupies positions `start..tokens.len()` and lines up with the
        // KV cells that survived the truncation.
        let mut batch = LlamaBatch::new(N_BATCH as usize, 1);
        let last = tokens.len().saturating_sub(1);
        let mut pos = start;
        for chunk in tokens[start..].chunks(N_BATCH as usize) {
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
        let mut generated = Vec::new();
        let mut n_cur = i32::try_from(tokens.len()).unwrap_or(i32::MAX);
        // One [`PieceDecoder`] for the whole stream — see its docs for why
        // the decoder must outlive every token (LESSON-452).
        let mut decoder = PieceDecoder::new();

        while completion_tokens < params.max_tokens {
            let token = sampler.sample(ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.is_eog_token(token) {
                break;
            }
            let piece = decoder.push(&piece_bytes(model, token)?);
            // A token that only *starts* a multi-byte character yields an
            // empty piece (its bytes are held in the decoder) — nothing to
            // stream yet, but it still counts as a generated token.
            let mut keep_going = true;
            if !piece.is_empty() {
                keep_going = emit(out_tx, ctrl_rx, piece.clone());
                text.push_str(&piece);
            }
            completion_tokens += 1;
            if !keep_going {
                // The caller ended the turn (e.g. the first tool call
                // completed); stop decoding — the tail flush below still
                // accounts for any held partial character.
                break;
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| EngineError::Backend(e.to_string()))?;
            n_cur += 1;
            generated.push(token);
            ctx.decode(&mut batch)
                .map_err(|e| EngineError::Backend(e.to_string()))?;
        }

        // The stream can end (EOG or max_tokens) while the decoder holds an
        // incomplete sequence; the flush turns it into U+FFFD rather than
        // dropping the bytes silently.
        let tail = decoder.finish();
        if !tail.is_empty() {
            emit(out_tx, ctrl_rx, tail.clone());
            text.push_str(&tail);
        }

        Ok(Generated {
            text,
            completion_tokens,
            tokens: generated,
        })
    }

    /// Hand one piece to the caller and wait for its continue/stop answer.
    ///
    /// A caller that has gone away (either channel closed) reads as "stop":
    /// there is nobody left to stream to, and unlike the old inline path there
    /// is no reason to keep burning inference for a result no one will receive.
    fn emit(out_tx: &Sender<Emission>, ctrl_rx: &Receiver<bool>, piece: String) -> bool {
        if out_tx.send(Emission::Token(piece)).is_err() {
            return false;
        }
        ctrl_rx.recv().unwrap_or(false)
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

    /// The guard's boundary, pinned exactly: a prompt *at* the budget is
    /// allowed and one token past it is refused. An off-by-one in the
    /// permissive direction here is a `GGML_ASSERT` process abort, not a bad
    /// answer (LESSON-444).
    #[test]
    fn the_window_guard_admits_the_budget_and_refuses_one_past_it() {
        // 16384 context minus 512 generation leaves a 15872-token budget.
        assert!(over_window(15872, 16384, 512).is_none());
        let refusal = over_window(15873, 16384, 512).expect("one past the budget is refused");
        let message = refusal.to_string();
        assert!(message.contains("15873"), "names the offending size");
        assert!(message.contains("15872"), "names the budget");
        assert!(message.contains("16384"), "names the context");
        assert!(message.contains("512"), "names the generation reserve");
    }

    /// A `max_tokens` larger than the whole window saturates to a zero budget
    /// rather than wrapping to a huge one — the wrap would admit every prompt.
    #[test]
    fn the_window_guard_saturates_when_generation_exceeds_the_context() {
        assert!(over_window(0, 512, 4096).is_none());
        assert!(over_window(1, 512, 4096).is_some());
    }

    /// `processed_tokens` is derived, never stored, so it cannot disagree with
    /// the two counts it is computed from (LESSON-446).
    #[test]
    fn processed_tokens_is_the_uncached_remainder() {
        let mut completion = Completion::cold("hi".to_owned(), 100, 5);
        assert_eq!(completion.processed_tokens(), 100);
        assert_eq!(completion.cached_tokens, 0);
        assert_eq!(completion.cache_miss, Some(MissReason::Cold));

        completion.cached_tokens = 80;
        assert_eq!(completion.processed_tokens(), 20);
    }

    /// The trait default is a *cold* completion, not a silent pretend-hit: an
    /// engine with no cache must say so rather than report `None` (BR-8).
    #[test]
    fn the_default_cached_completion_reports_a_cold_miss() {
        let mut engine = MockEngine::new("mock-3b");
        let completion = engine
            .complete_cached(
                "session-1",
                "hello there",
                &GenParams::default(),
                &mut |_| true,
            )
            .expect("the default delegates to complete");
        assert_eq!(completion.cached_tokens, 0);
        assert_eq!(completion.cache_miss, Some(MissReason::Cold));
    }

    /// Evicting an engine that holds no cache is a no-op, not a panic — BR-4's
    /// "a dropped cache must never fail a turn" starts here.
    #[test]
    fn evicting_a_cacheless_engine_is_harmless() {
        let mut engine = MockEngine::new("mock-3b");
        engine.evict_prefix_cache(crate::prefix_cache::EvictionReason::MemoryPressure);
        assert!(engine
            .complete("still works", &GenParams::default(), &mut |_| true)
            .is_ok());
    }

    #[test]
    fn mock_streams_tokens_and_counts_them() {
        let engine = MockEngine::new("mock-3b");
        let mut streamed = String::new();
        let completion = engine
            .complete("hello there world", &GenParams::default(), &mut |t| {
                streamed.push_str(t);
                true
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
            .complete("same prompt", &GenParams::default(), &mut |_| true)
            .unwrap();
        let b = engine
            .complete("same prompt", &GenParams::default(), &mut |_| true)
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
        let completion = engine
            .complete("x", &params, &mut |_| {
                count += 1;
                true
            })
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(completion.completion_tokens, 3);
    }

    #[test]
    fn a_false_from_on_token_stops_generation_early() {
        // BUG-147: the harness ends a turn at the first complete tool call by
        // returning `false`; the completion's text is what was emitted, not the
        // full response the model would have gone on to fabricate.
        let engine = MockEngine::with_response("mock", "one two three four five six seven");
        let mut seen = String::new();
        let completion = engine
            .complete("x", &GenParams::default(), &mut |t| {
                seen.push_str(t);
                !seen.contains("three")
            })
            .unwrap();
        assert_eq!(completion.text, "one two three ");
        assert_eq!(completion.text, seen);
        assert_eq!(completion.completion_tokens, 3);
    }

    #[test]
    fn unavailable_mock_returns_the_typed_error() {
        let engine = MockEngine::unavailable("mock-3b", "unloaded under memory pressure");
        let err = engine
            .complete("anything", &GenParams::default(), &mut |_| true)
            .unwrap_err();
        match err {
            EngineError::Unavailable { reason } => {
                assert!(reason.contains("memory pressure"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        // The Display form is the user-facing "local tier unavailable" string.
        assert!(engine
            .complete("x", &GenParams::default(), &mut |_| true)
            .unwrap_err()
            .to_string()
            .starts_with("local tier unavailable"));
    }

    /// A ChatML dialect this renderer cannot reproduce falls back to Flat:
    /// Phi-4's template CONTAINS `<|im_start|>` but separates role from
    /// content with `<|im_sep|>` where our renderer writes a newline —
    /// rendering it as plain ChatML would silently feed the model a format it
    /// was not trained on, with no further fallback (REQ-554 verify).
    #[test]
    fn a_chatml_dialect_with_im_sep_falls_back_to_flat() {
        let template = "{% for message in messages %}\
                        <|im_start|>{{ message['role'] }}<|im_sep|>\
                        {{ message['content'] }}<|im_end|>{% endfor %}\
                        <|im_start|>assistant<|im_sep|>";
        assert_eq!(detect_chat_format(template), ChatFormat::Flat);
    }

    /// A template that merely *mentions* the opening delimiter without the
    /// closing one is not ChatML — detection is corroborated, not a single
    /// substring (REQ-554 verify).
    #[test]
    fn a_template_mentioning_im_start_alone_falls_back_to_flat() {
        let template = "{# legacy: some models used <|im_start|> here #}\
                        {% for m in messages %}[INST]{{ m['content'] }}[/INST]{% endfor %}";
        assert_eq!(detect_chat_format(template), ChatFormat::Flat);
    }

    /// A ChatML template is recognized from its role delimiters — no
    /// Jinja evaluation, no `llama` feature, no weights (REQ-554 ADR-1/AC-8).
    #[test]
    fn a_chatml_template_string_is_detected() {
        let template = "{%- if messages %}\n\
                        {%- for message in messages %}\n\
                        <|im_start|>{{ message.role }}\n{{ message.content }}<|im_end|>\n\
                        {%- endfor %}\n\
                        {%- endif %}\n\
                        <|im_start|>assistant\n";
        assert_eq!(detect_chat_format(template), ChatFormat::ChatMl);
    }

    /// Absent template metadata reaches the matcher as an empty string; the
    /// fallback is the answer, never a failure (REQ-554 BR-2/BR-6).
    #[test]
    fn an_empty_template_falls_back_to_flat() {
        assert_eq!(detect_chat_format(""), ChatFormat::Flat);
    }

    /// A real template of another family is recognized as *unrecognized*: it
    /// takes the flat fallback rather than being rendered as ChatML.
    #[test]
    fn a_non_chatml_template_falls_back_to_flat() {
        // Llama-2 style: [INST]/<<SYS>> delimiters, no `<|im_start|>`.
        let template = "{% for message in messages %}\
                        {% if message['role'] == 'system' %}\
                        {{ '<<SYS>>\\n' + message['content'] + '\\n<</SYS>>\\n\\n' }}\
                        {% else %}{{ '[INST] ' + message['content'] + ' [/INST]' }}\
                        {% endif %}{% endfor %}";
        assert_eq!(detect_chat_format(template), ChatFormat::Flat);
    }

    /// The trait default is what keeps every scripted, gated, and mock engine
    /// on the flat rendering without an edit (REQ-554 ADR-2).
    #[test]
    fn an_engine_reports_flat_by_default() {
        assert_eq!(MockEngine::new("mock-3b").chat_format(), ChatFormat::Flat);
    }

    /// The builder is how a wiring test simulates a template-bearing engine.
    #[test]
    fn the_mock_builder_overrides_the_chat_format() {
        let engine =
            MockEngine::with_response("mock-3b", "ok").with_chat_format(ChatFormat::ChatMl);
        assert_eq!(engine.chat_format(), ChatFormat::ChatMl);
        // The builder changes metadata only — generation is untouched.
        let completion = engine
            .complete("x", &GenParams::default(), &mut |_| true)
            .unwrap();
        assert_eq!(completion.text, "ok");
    }
}
