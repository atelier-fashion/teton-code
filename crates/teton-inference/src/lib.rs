//! teton-inference — local model lifecycle.
//!
//! The local tier is the always-on cheap tier: routing/intent classification,
//! file and diff summarization, grep triage, commit messages, and the offline
//! fallback. Its value is *latency*, not intelligence — and it must never
//! degrade the machine. This crate owns that lifecycle end to end (BR-8/BR-9):
//!
//! - [`engine`] — the [`Engine`] trait abstracting inference, a deterministic
//!   [`MockEngine`], and (behind the non-default `llama` feature) the real
//!   `LlamaEngine` llama.cpp binding.
//! - [`probe`] — the first-run hardware probe and the OQ-3 decision table
//!   ([`decide`]): RAM/disk/GPU class → a candidate model, or the cleanly
//!   disabled tier below the 8 GiB floor.
//! - [`catalog`] — the versioned model catalog ([`Catalog`]), authored as TOML
//!   *data* the daemon can update independently of releases.
//! - [`download`] — resumable, checksum-verified GGUF download
//!   ([`Downloader`]) with progress events.
//! - [`benchmark`] — the post-download micro-benchmark and auto-step-down
//!   ([`benchmark_with_step_down`]) that enforces the BR-8 latency duty.
//! - [`prefix_cache`] — the REQ-564 prefix-reuse *decision* ([`PrefixCacheState`]):
//!   a pure, feature-free policy over token ids, kept apart from the gated
//!   llama.cpp mechanism so CI can prove the rule it enforces.
//! - [`pressure`] — the runtime memory-pressure watcher
//!   ([`PressureController`]) that downgrades or unloads under load and reloads
//!   on recovery.
//! - [`lifecycle`] — the [`LifecycleEvent`] progress type shared by all of the
//!   above, mapping onto the daemon's `model_lifecycle` protocol event.
//! - [`window`] — the REQ-616 local-window decision ([`fit_window`]): trained
//!   window, KV cache type and admissible RAM resolved to the context the
//!   daemon loads with. Pure, like [`probe::decide`], so CI can prove the rule
//!   without a machine that could hold the model.
//!
//! ## The `llama` feature
//!
//! The real llama.cpp binding (`llama-cpp-2`) is an **optional dependency gated
//! behind the non-default `llama` feature**. Default builds and CI compile only
//! the [`Engine`] trait and [`MockEngine`] — never llama.cpp, and so never
//! require cmake. Build with `--features llama` (which needs cmake and, at
//! runtime, a real GGUF) to compile the real engine and its `#[ignore]`d smoke
//! test.

pub mod benchmark;
pub mod catalog;
pub mod download;
pub mod engine;
mod hash;
pub mod lifecycle;
pub mod prefix_cache;
pub mod pressure;
pub mod probe;
pub mod window;

pub use benchmark::{
    benchmark_with_step_down, default_prompts, run_benchmark, BenchmarkResult, BenchmarkSelection,
    DutyOutcome, DutySpec,
};
pub use catalog::{Catalog, ModelEntry, TierBand};
pub use download::{DownloadConfig, DownloadError, Downloader, RangeFetcher};
pub use engine::{
    detect_chat_format, over_window, ChatFormat, Completion, Engine, EngineError, GenParams,
    MockEngine,
};
/// SHA-256 over files and byte slices — the integrity primitive behind BR-6.
///
/// Re-exported (rather than the whole `hash` module made public) so the daemon's
/// install pipeline can re-verify an already-installed artifact against the same
/// implementation this crate uses (REQ-547 BR-9's `InstallState`) without
/// exposing the hand-rolled streaming `Sha256` internals as public crate API.
pub use hash::{sha256_file, sha256_hex};
pub use lifecycle::LifecycleEvent;
pub use prefix_cache::{CacheDecision, EvictionReason, MissReason, PrefixCacheState};
pub use pressure::{MemoryPressure, PressureController};
pub use probe::{band_for_ram, decide, probe, GpuClass, HardwareProfile, TierDecision};
pub use window::{
    admissible_bytes, fit_window, kv_bytes_per_token, minimum_unconfigured_window, KvCacheType,
    WindowDecision, WindowFitInputs, WindowReason, ADMISSIBLE_RAM_PERCENT,
    MEASURED_KV_BYTES_PER_TOKEN_F16, WINDOW_STEP,
};

#[cfg(feature = "llama")]
pub use engine::LlamaEngine;

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
