//! Post-download micro-benchmark and auto-step-down (BR-8 / BR-9, AC-8).
//!
//! The local tier's value is *latency*, not intelligence: it must answer its
//! classification/summarization duties with visible latency of about a second
//! or the router bypasses it (BR-8). After download, [`run_benchmark`] measures
//! first-token latency and decode throughput on representative prompts;
//! [`DutySpec::evaluate`] judges the result; and [`benchmark_with_step_down`]
//! walks the catalog to a smaller model when the duty fails, terminating cleanly
//! at *disabled* rather than looping.

use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::engine::{Engine, EngineError, GenParams};
use crate::lifecycle::LifecycleEvent;
use crate::probe::HardwareProfile;

/// A measured benchmark outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchmarkResult {
    /// Time to first token, in milliseconds.
    pub first_token_ms: u32,
    /// Decode throughput, in tokens per second.
    pub tokens_per_sec: f32,
}

/// The latency duty a local model must satisfy (BR-8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DutySpec {
    /// Maximum acceptable time to first token, in milliseconds.
    pub max_first_token_ms: u32,
    /// Minimum acceptable decode throughput, in tokens per second.
    pub min_tokens_per_sec: f32,
}

impl Default for DutySpec {
    fn default() -> Self {
        // BR-8's "visible latency of about a second" for the first token, plus a
        // modest throughput floor so a fast-first-token-but-glacial-decode model
        // still fails.
        Self {
            max_first_token_ms: 1000,
            min_tokens_per_sec: 5.0,
        }
    }
}

/// Whether a [`BenchmarkResult`] met the duty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DutyOutcome {
    /// The model met the latency duty.
    Pass,
    /// The model failed; `reason` is user-facing.
    Fail {
        /// Why the duty failed.
        reason: String,
    },
}

impl DutyOutcome {
    /// Whether the duty passed.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, DutyOutcome::Pass)
    }
}

impl DutySpec {
    /// Judge `result` against this duty.
    #[must_use]
    pub fn evaluate(&self, result: &BenchmarkResult) -> DutyOutcome {
        if result.first_token_ms > self.max_first_token_ms {
            return DutyOutcome::Fail {
                reason: format!(
                    "first-token latency {}ms exceeds the {}ms duty (BR-8)",
                    result.first_token_ms, self.max_first_token_ms
                ),
            };
        }
        if result.tokens_per_sec < self.min_tokens_per_sec {
            return DutyOutcome::Fail {
                reason: format!(
                    "decode throughput {:.1} tok/s is below the {:.1} tok/s floor",
                    result.tokens_per_sec, self.min_tokens_per_sec
                ),
            };
        }
        DutyOutcome::Pass
    }
}

/// The result of benchmarking with step-down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchmarkSelection {
    /// This model met the duty and is selected.
    Selected(String),
    /// No model met the duty; the local tier is disabled.
    Disabled {
        /// User-facing reason.
        reason: String,
    },
}

/// The representative prompts the benchmark runs: one classification duty and
/// one summarization duty (the local tier's actual jobs).
#[must_use]
pub fn default_prompts() -> [&'static str; 2] {
    [
        "Classify the intent of this request as one of {edit, ask, run, search}: \
         'add a null check to the parser'.",
        "Summarize this diff in one sentence: '- let x = 1; + let x = compute();'.",
    ]
}

/// Run a live micro-benchmark of `engine` over `prompts`, measuring first-token
/// latency and aggregate decode throughput.
///
/// # Errors
/// Propagates any [`EngineError`] from the engine (e.g. the tier went
/// unavailable mid-benchmark).
pub fn run_benchmark(
    engine: &dyn Engine,
    prompts: &[&str],
    params: &GenParams,
) -> Result<BenchmarkResult, EngineError> {
    let start = Instant::now();
    let mut first_token: Option<Duration> = None;
    let mut total_tokens: u64 = 0;

    for prompt in prompts {
        let completion = engine.complete(prompt, params, &mut |_token| {
            if first_token.is_none() {
                first_token = Some(start.elapsed());
            }
        })?;
        total_tokens += u64::from(completion.completion_tokens);
    }

    // Floor the elapsed time so throughput stays finite for instantaneous mocks.
    let elapsed_secs = start.elapsed().as_secs_f32().max(1e-6);
    let first_token_ms = first_token.map_or(u32::MAX, |d| {
        u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
    });
    let tokens_per_sec = total_tokens as f32 / elapsed_secs;

    Ok(BenchmarkResult {
        first_token_ms,
        tokens_per_sec,
    })
}

/// Benchmark `start_model`, stepping down to the next smaller catalog model each
/// time the duty fails, until one passes or the chain is exhausted.
///
/// `measure` supplies a [`BenchmarkResult`] for a given model — in production it
/// loads the model and calls [`run_benchmark`]; in tests it returns synthetic
/// results for deterministic step-down coverage. Progress is reported through
/// `on_event` as `Benchmark`, `SteppedDown`, `Ready`, and `Disabled`
/// `model_lifecycle` events.
///
/// Termination is guaranteed: [`Catalog::step_down_from`] returns a strictly
/// smaller model each time and the catalog is finite, so the walk cannot loop.
pub fn benchmark_with_step_down(
    catalog: &Catalog,
    profile: &HardwareProfile,
    start_model: &str,
    duty: &DutySpec,
    mut measure: impl FnMut(&crate::catalog::ModelEntry) -> BenchmarkResult,
    on_event: &mut dyn FnMut(LifecycleEvent),
) -> BenchmarkSelection {
    let mut current = start_model.to_owned();
    // Defensive bound: even if a future catalog were mis-ordered, never loop
    // beyond the number of models.
    let max_steps = catalog.models.len() + 1;

    for _ in 0..=max_steps {
        let Some(model) = catalog.get(&current) else {
            return BenchmarkSelection::Disabled {
                reason: format!("model '{current}' is not in the catalog"),
            };
        };

        let result = measure(model);
        on_event(LifecycleEvent::Benchmark {
            model_id: model.name.clone(),
            first_token_ms: result.first_token_ms,
            tokens_per_sec: result.tokens_per_sec,
        });

        match duty.evaluate(&result) {
            DutyOutcome::Pass => {
                on_event(LifecycleEvent::Ready {
                    model_id: model.name.clone(),
                });
                return BenchmarkSelection::Selected(model.name.clone());
            }
            DutyOutcome::Fail { reason } => match catalog.step_down_from(&model.name, profile) {
                Some(next) => {
                    on_event(LifecycleEvent::SteppedDown {
                        from_model: model.name.clone(),
                        to_model: next.name.clone(),
                        reason,
                    });
                    current = next.name.clone();
                }
                None => {
                    let reason =
                        "no local model met the latency duty; disabling the local tier".to_owned();
                    on_event(LifecycleEvent::Disabled {
                        reason: reason.clone(),
                    });
                    return BenchmarkSelection::Disabled { reason };
                }
            },
        }
    }

    // Unreachable given strict step-down; the bound is purely a loop guard.
    BenchmarkSelection::Disabled {
        reason: "benchmark step-down exceeded the catalog depth".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, ModelEntry};
    use crate::engine::MockEngine;
    use crate::probe::{GpuClass, HardwareProfile};

    fn big_machine() -> HardwareProfile {
        HardwareProfile {
            ram_bytes: 64 * 1024 * 1024 * 1024,
            free_disk_bytes: 500 * 1_000_000_000,
            gpu: GpuClass::AppleSilicon,
        }
    }

    fn fast() -> BenchmarkResult {
        BenchmarkResult {
            first_token_ms: 200,
            tokens_per_sec: 40.0,
        }
    }

    fn slow() -> BenchmarkResult {
        BenchmarkResult {
            first_token_ms: 2500,
            tokens_per_sec: 3.0,
        }
    }

    #[test]
    fn duty_passes_a_fast_result() {
        assert!(DutySpec::default().evaluate(&fast()).is_pass());
    }

    #[test]
    fn duty_fails_slow_first_token() {
        let r = BenchmarkResult {
            first_token_ms: 1500,
            tokens_per_sec: 40.0,
        };
        match DutySpec::default().evaluate(&r) {
            DutyOutcome::Fail { reason } => assert!(reason.contains("first-token")),
            DutyOutcome::Pass => panic!("should fail on latency"),
        }
    }

    #[test]
    fn duty_fails_low_throughput() {
        let r = BenchmarkResult {
            first_token_ms: 100,
            tokens_per_sec: 1.0,
        };
        match DutySpec::default().evaluate(&r) {
            DutyOutcome::Fail { reason } => assert!(reason.contains("throughput")),
            DutyOutcome::Pass => panic!("should fail on throughput"),
        }
    }

    #[test]
    fn live_benchmark_of_the_mock_passes_the_duty() {
        let engine = MockEngine::new("mock-3b");
        let prompts = default_prompts();
        let result = run_benchmark(&engine, &prompts, &GenParams::default()).unwrap();
        assert!(DutySpec::default().evaluate(&result).is_pass());
    }

    #[test]
    fn passing_start_model_is_selected_without_stepping_down() {
        let catalog = Catalog::bundled();
        let mut events = Vec::new();
        let selection = benchmark_with_step_down(
            &catalog,
            &big_machine(),
            "qwen2.5-coder-7b",
            &DutySpec::default(),
            |_model| fast(),
            &mut |e| events.push(e),
        );
        assert_eq!(
            selection,
            BenchmarkSelection::Selected("qwen2.5-coder-7b".to_owned())
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, LifecycleEvent::Ready { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, LifecycleEvent::SteppedDown { .. })));
    }

    #[test]
    fn step_down_stops_at_the_first_model_that_passes() {
        let catalog = Catalog::bundled();
        // Slow for the 7b, fast for anything smaller: expect one step to the 3b.
        let measure = |model: &ModelEntry| {
            if model.name == "qwen2.5-coder-7b" {
                slow()
            } else {
                fast()
            }
        };
        let mut events = Vec::new();
        let selection = benchmark_with_step_down(
            &catalog,
            &big_machine(),
            "qwen2.5-coder-7b",
            &DutySpec::default(),
            measure,
            &mut |e| events.push(e),
        );
        assert_eq!(
            selection,
            BenchmarkSelection::Selected("qwen2.5-coder-3b".to_owned())
        );
        let stepped = events
            .iter()
            .filter(|e| matches!(e, LifecycleEvent::SteppedDown { .. }))
            .count();
        assert_eq!(stepped, 1, "exactly one step-down expected");
    }

    #[test]
    fn all_slow_walks_the_whole_chain_to_disabled_without_looping() {
        let catalog = Catalog::bundled();
        let mut events = Vec::new();
        let selection = benchmark_with_step_down(
            &catalog,
            &big_machine(),
            "qwen2.5-coder-7b",
            &DutySpec::default(),
            |_model| slow(),
            &mut |e| events.push(e),
        );
        assert!(matches!(selection, BenchmarkSelection::Disabled { .. }));

        // 7b -> 3b -> 1.5b -> disabled: two step-downs, then a disabled event.
        let stepped = events
            .iter()
            .filter(|e| matches!(e, LifecycleEvent::SteppedDown { .. }))
            .count();
        assert_eq!(stepped, 2);
        assert!(matches!(
            events.last(),
            Some(LifecycleEvent::Disabled { .. })
        ));
        // Benchmark ran once per model in the chain (3 models), never repeating.
        let benched = events
            .iter()
            .filter(|e| matches!(e, LifecycleEvent::Benchmark { .. }))
            .count();
        assert_eq!(benched, 3);
    }

    #[test]
    fn unknown_start_model_disables_cleanly() {
        let catalog = Catalog::bundled();
        let selection = benchmark_with_step_down(
            &catalog,
            &big_machine(),
            "ghost",
            &DutySpec::default(),
            |_model| fast(),
            &mut |_| {},
        );
        assert!(matches!(selection, BenchmarkSelection::Disabled { .. }));
    }
}
