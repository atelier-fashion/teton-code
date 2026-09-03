//! REQ-590 AC-10 — what a full-window local prompt costs, measured on real weights.
//!
//! **This is a recorded measurement, not a CI assertion.** The real engine lives
//! behind `--features llama`, which compiles llama.cpp from source, and it needs
//! the GGUF weights present on disk. Neither is true of the default build, so
//! this is an example run by hand whose numbers are pasted into
//! `.adlc/specs/REQ-590-engine-derived-local-context-budget/architecture.md`
//! (§ Measurements) with the machine and date they were taken on. The runbook
//! is `docs/manual-verification.md`, "REQ-590 — the engine-derived local
//! budget".
//!
//! ```text
//! cargo run --release -p teton-inference --features llama \
//!     --example local_budget_cost -- <path-to.gguf>
//! ```
//!
//! ## What it measures
//!
//! * **AC-10(a)** wall-clock prefill — time to first token, with generation
//!   capped at a handful of tokens so decode is noise — for a prompt at the
//!   derived budget (31,744 tokens: `LOCAL_ENGINE_N_CTX` less the 1,024-token
//!   generation reservation) against one at the previous window's budget
//!   (15,360 tokens: the 16,384-token window less the same reservation).
//!   REQ-590 measured prefill as **super-linear** on this engine (6,164 →
//!   15,410 tokens cost 3,111 → 13,548 ms, 4.35× for 2.5×), so the ratio to
//!   expect for this 2.07× step is well over 2×; the number is the finding.
//!
//! * **AC-10(b)** the REQ-544 BR-8 duty ([`DutySpec::default`] —
//!   `min_tokens_per_sec: 5.0`, `max_first_token_ms: 1000`) run twice: verbatim
//!   on the short duty prompts it ships against, and again with a full-budget
//!   context resident in front of each duty prompt. Pass = the duty still
//!   passes.
//!
//!   [`run_benchmark`]'s `tokens_per_sec` is **prefill-inclusive** — generated
//!   tokens divided by the whole wall clock, prefill included — so a large
//!   resident context depresses it without any decode having slowed down. The
//!   loaded run therefore also reports a decode-only rate, which is what
//!   "generation under a large resident context" actually asks for. Both are
//!   printed; neither is silently substituted for the other.
//!
//! ## Why the prompts are calibrated rather than assumed
//!
//! Token counts come from the engine's own `prompt_tokens`, not from
//! `approx_tokens`: an assertion about a 15,360-token prompt written in
//! whitespace words is the exact blindness REQ-590 AC-9 exists to close. One
//! cheap probe measures tokens-per-filler-unit on the loaded model, and the two
//! target prompts are sized from that measurement.

#[cfg(not(feature = "llama"))]
fn main() {
    eprintln!(
        "local_budget_cost measures the real engine, which is behind a non-default feature.\n\
         Re-run with: cargo run --release -p teton-inference --features llama \\\n\
         \x20   --example local_budget_cost -- <path-to.gguf>"
    );
    std::process::exit(2);
}

#[cfg(feature = "llama")]
fn main() -> std::process::ExitCode {
    match llama::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("measurement failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "llama")]
mod llama {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use teton_inference::{
        default_prompts, run_benchmark, DutySpec, Engine, EngineError, GenParams, LlamaEngine,
    };

    /// `LOCAL_ENGINE_N_CTX` (`tetond/src/runtime.rs`) — the window the daemon
    /// loads with. Duplicated rather than imported because it is `pub(crate)` in
    /// a crate this one does not depend on; a measurement taken against a
    /// *different* window would be measuring nothing, so it is printed in the
    /// report for the reader to check against the source.
    const N_CTX: u32 = 32_768;

    /// `LOCAL_GENERATION_RESERVATION` (`tetond/src/harness/budget.rs`, TASK-269).
    const RESERVATION: u32 = 1_024;

    /// The budget the derivation gives this window: `N_CTX - RESERVATION`, in
    /// real tokens.
    const AFTER_TOKENS: u32 = N_CTX - RESERVATION;

    /// The same derivation on the 16,384-token window the daemon loaded with
    /// before: `16,384 - 1,024`. (REQ-590's original run compared 15,360
    /// against the pre-derivation 6,144 — 4,096 whitespace words × 3 ÷ 2.)
    const BEFORE_TOKENS: u32 = 16_384 - RESERVATION;

    /// Prefill probes per size. The first is discarded as a warmup (Metal
    /// pipeline construction lands on it), and the median of the rest reported.
    const PROBES: usize = 4;

    /// Generation cap for a prefill probe: enough to get a first token, few
    /// enough that decode is noise against a multi-second prefill.
    const PREFILL_MAX_TOKENS: u32 = 4;

    /// One repetition of the filler corpus: source-shaped text, because a local
    /// coding session's context is mostly code and code is the dense end of the
    /// ratio (`budget.rs` measures Rust at 1.69 tokens/whitespace-word).
    ///
    /// Prefill cost is per *token*, so the content class changes how many words
    /// a token target corresponds to, not what a token costs to prefill. The
    /// targets below are in tokens for exactly that reason.
    const FILLER_UNIT: &str = "\
fn resolve_route(&self, session: &SessionId, hint: Option<&RouteHint>) -> RouteDecision {
    let budget = self.budget_for(session).unwrap_or_else(RouteBudget::local);
    if budget.estimated_tokens() > budget.budget_tokens {
        return RouteDecision::Refused { reason: Refusal::OverBudget, budget };
    }
    let tier = match hint {
        Some(RouteHint::Local) if self.local_available() => Tier::Local,
        Some(RouteHint::Remote(provider)) => Tier::Remote(provider.clone()),
        _ => self.default_tier(),
    };
    RouteDecision::Serve { tier, budget }
}
";

    /// What one prefill probe saw.
    struct Probe {
        first_token: Duration,
        total: Duration,
        prompt_tokens: u32,
        completion_tokens: u32,
        cached_tokens: u32,
    }

    impl Probe {
        /// Tokens per second of *decode*, with prefill excluded — the rate
        /// `run_benchmark` does not isolate.
        fn decode_tokens_per_sec(&self) -> f64 {
            let decode = self.total.saturating_sub(self.first_token).as_secs_f64();
            if decode <= 0.0 {
                return f64::INFINITY;
            }
            f64::from(self.completion_tokens) / decode
        }
    }

    /// Drive one cold completion, timing the first token.
    fn probe(engine: &dyn Engine, prompt: &str, max_tokens: u32) -> Result<Probe, EngineError> {
        let params = GenParams {
            max_tokens,
            temperature: 0.0,
        };
        let start = Instant::now();
        let mut first_token: Option<Duration> = None;
        let completion = engine.complete(prompt, &params, &mut |_token| {
            if first_token.is_none() {
                first_token = Some(start.elapsed());
            }
            true
        })?;
        let total = start.elapsed();
        Ok(Probe {
            first_token: first_token.unwrap_or(total),
            total,
            prompt_tokens: completion.prompt_tokens,
            completion_tokens: completion.completion_tokens,
            cached_tokens: completion.cached_tokens,
        })
    }

    fn filler(units: usize) -> String {
        FILLER_UNIT.repeat(units)
    }

    fn median_ms(mut samples: Vec<u128>) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    /// Milliseconds as `f64` for the ratio arithmetic. Wall-clock millis on this
    /// scale are nowhere near `f64`'s exact-integer range, so the conversion is
    /// lossless here and the cast is named rather than sprinkled inline.
    fn ms_as_f64(ms: u128) -> f64 {
        u32::try_from(ms).map_or(f64::INFINITY, f64::from)
    }

    /// Filler units needed to reach `target` real tokens, from the measured
    /// tokens-per-unit.
    fn units_for(target: u32, tokens_per_unit: f64) -> usize {
        let units = (f64::from(target) / tokens_per_unit).round().max(1.0);
        // The targets here are ~15k tokens against ~100 tokens/unit, so this is
        // a few hundred; the clamp is defensive, not load-bearing.
        units.min(1_000_000.0) as usize
    }

    /// Measure prefill at `target` tokens, reporting the median of `PROBES - 1`
    /// timed runs after one discarded warmup.
    fn prefill_at(
        engine: &dyn Engine,
        target: u32,
        tokens_per_unit: f64,
    ) -> Result<(u32, u128, Vec<u128>), EngineError> {
        let prompt = filler(units_for(target, tokens_per_unit));

        let mut measured = Vec::with_capacity(PROBES - 1);
        let mut observed_tokens = 0;
        for round in 0..PROBES {
            let p = probe(engine, &prompt, PREFILL_MAX_TOKENS)?;
            observed_tokens = p.prompt_tokens;
            if p.cached_tokens != 0 {
                eprintln!(
                    "  warning: round {round} reused {} KV tokens — this is not a cold prefill",
                    p.cached_tokens
                );
            }
            if round == 0 {
                println!(
                    "    warmup (discarded): {} ms, {} prompt tokens",
                    p.first_token.as_millis(),
                    p.prompt_tokens
                );
            } else {
                measured.push(p.first_token.as_millis());
            }
        }
        let median = median_ms(measured.clone());
        Ok((observed_tokens, median, measured))
    }

    /// [`prefill_at`] without the per-round chatter, for the sweep — one warmup,
    /// two timed rounds, median reported.
    fn prefill_at_quiet(
        engine: &dyn Engine,
        target: u32,
        tokens_per_unit: f64,
    ) -> Result<(u32, u128, Vec<u128>), EngineError> {
        let prompt = filler(units_for(target, tokens_per_unit));
        let mut measured = Vec::with_capacity(2);
        let mut observed_tokens = 0;
        for round in 0..3 {
            let p = probe(engine, &prompt, PREFILL_MAX_TOKENS)?;
            observed_tokens = p.prompt_tokens;
            if round > 0 {
                measured.push(p.first_token.as_millis());
            }
        }
        let median = median_ms(measured.clone());
        Ok((observed_tokens, median, measured))
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let path: PathBuf = std::env::args_os()
            .nth(1)
            .ok_or("usage: local_budget_cost <path-to.gguf>")?
            .into();
        if !path.is_file() {
            return Err(format!("no weights at {}", path.display()).into());
        }

        println!("REQ-590 AC-10 — local context budget cost");
        println!("weights:      {}", path.display());
        println!("n_ctx:        {N_CTX} (LOCAL_ENGINE_N_CTX)");
        println!("reservation:  {RESERVATION} (LOCAL_GENERATION_RESERVATION)");
        println!("before/after: {BEFORE_TOKENS} → {AFTER_TOKENS} prompt tokens");
        println!();

        let load_start = Instant::now();
        // `u32::MAX` layers = the Metal fast path, matching what
        // `LlamaEngineLoader` requests on Apple Silicon (`runtime.rs`).
        let engine = LlamaEngine::load("local-budget-cost", &path, u32::MAX, N_CTX)?;
        println!("model loaded in {} ms", load_start.elapsed().as_millis());
        println!("model_id: {}", engine.model_id());
        println!();

        // Calibrate: how many real tokens is one filler unit? Measured on the
        // loaded tokenizer, never estimated.
        let calibration_units: u32 = 64;
        let calibration = probe(&engine, &filler(calibration_units as usize), 1)?;
        let tokens_per_unit = f64::from(calibration.prompt_tokens) / f64::from(calibration_units);
        println!(
            "calibration: {calibration_units} filler units = {} tokens ({tokens_per_unit:.2} tokens/unit)",
            calibration.prompt_tokens
        );
        println!();

        println!("## AC-10(a) — prefill wall clock");
        println!("  today's budget ({BEFORE_TOKENS} tokens):");
        let (before_tokens, before_ms, before_all) =
            prefill_at(&engine, BEFORE_TOKENS, tokens_per_unit)?;
        println!("    {before_tokens} prompt tokens, median first token {before_ms} ms, samples {before_all:?}");

        println!("  derived budget ({AFTER_TOKENS} tokens):");
        let (after_tokens, after_ms, after_all) =
            prefill_at(&engine, AFTER_TOKENS, tokens_per_unit)?;
        println!("    {after_tokens} prompt tokens, median first token {after_ms} ms, samples {after_all:?}");

        let token_ratio = f64::from(after_tokens) / f64::from(before_tokens);
        let before_secs = ms_as_f64(before_ms.max(1));
        let after_secs = ms_as_f64(after_ms);
        let time_ratio = after_secs / before_secs;
        println!("  token ratio: {token_ratio:.2}×   prefill time ratio: {time_ratio:.2}×");
        println!(
            "  per-token prefill: {:.3} ms/token before, {:.3} ms/token after",
            before_secs / f64::from(before_tokens),
            after_secs / f64::from(after_tokens)
        );
        println!();

        // Two points cannot tell "steeper than linear" from "one of them was
        // noisy". A sweep can, and the shape is the whole finding: attention
        // prefill is linear in tokens *plus* quadratic in tokens, so the second
        // term is invisible at 6k and is not at 15k.
        println!("## AC-10(a) — the shape, swept");
        println!("  | target tokens | prompt tokens | median first token | ms/token |");
        println!("  |---|---|---|---|");
        for target in [6_144u32, 12_288, 18_432, 24_576, 31_744] {
            let (tokens, ms, _) = prefill_at_quiet(&engine, target, tokens_per_unit)?;
            println!(
                "  | {target} | {tokens} | {ms} ms | {:.3} |",
                ms_as_f64(ms) / f64::from(tokens)
            );
        }
        println!();

        println!("## AC-10(b) — the REQ-544 BR-8 duty");
        let duty = DutySpec::default();
        let params = GenParams::default();

        let short = default_prompts();
        let short_result = run_benchmark(&engine, &short, &params)?;
        println!("  duty prompts as shipped (short):");
        println!(
            "    first_token {} ms, {:.2} tok/s (prefill-inclusive) → {:?}",
            short_result.first_token_ms,
            short_result.tokens_per_sec,
            duty.evaluate(&short_result)
        );

        // The same duty prompts, each behind a full-budget resident context. The
        // duty's own `max_tokens` (256) must still fit: prompt + 256 <= n_ctx.
        let head_target = AFTER_TOKENS - params.max_tokens;
        let head = filler(units_for(head_target, tokens_per_unit));
        let loaded: Vec<String> = short.iter().map(|p| format!("{head}\n\n{p}")).collect();
        let loaded_refs: Vec<&str> = loaded.iter().map(String::as_str).collect();

        let loaded_result = run_benchmark(&engine, &loaded_refs, &params)?;
        println!("  duty prompts behind a full-budget context (~{head_target} tokens):");
        println!(
            "    first_token {} ms, {:.2} tok/s (prefill-inclusive) → {:?}",
            loaded_result.first_token_ms,
            loaded_result.tokens_per_sec,
            duty.evaluate(&loaded_result)
        );

        // Prefill-inclusive throughput cannot distinguish "decode got slower"
        // from "there was more to prefill". Measure decode on its own.
        println!("  decode-only rate, same prompts, prefill excluded:");
        for (i, prompt) in loaded_refs.iter().enumerate() {
            let p = probe(&engine, prompt, params.max_tokens)?;
            println!(
                "    prompt {i}: {} prompt tokens, first token {} ms, {} generated, {:.2} tok/s decode",
                p.prompt_tokens,
                p.first_token.as_millis(),
                p.completion_tokens,
                p.decode_tokens_per_sec()
            );
        }
        for (i, prompt) in short.iter().enumerate() {
            let p = probe(&engine, prompt, params.max_tokens)?;
            println!(
                "    short {i}: {} prompt tokens, first token {} ms, {} generated, {:.2} tok/s decode",
                p.prompt_tokens,
                p.first_token.as_millis(),
                p.completion_tokens,
                p.decode_tokens_per_sec()
            );
        }

        println!();
        println!(
            "done — paste these into architecture.md § Measurements with the machine and date"
        );
        Ok(())
    }
}
