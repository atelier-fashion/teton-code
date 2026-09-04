//! The local engine's window decision (REQ-616): how wide a context the daemon
//! loads with, and at which KV cache type.
//!
//! [`fit_window`] is a **pure function of its inputs** — no RAM detection, no
//! GGUF read, no clock — on the [`crate::probe::decide`] precedent, whose module
//! doc states the reason: "the runtime detection is deliberately factored out so
//! tests never depend on the host machine". The caller supplies the model's
//! trained window, its weight size, and how many bytes this machine may spend;
//! this module decides.
//!
//! Keeping it pure is also the only shape in which REQ-616 AC-5 means what it
//! says. Emulating RAM end to end would re-run *model selection*: at 16 GiB
//! [`crate::probe::band_for_ram`] yields the small band and the 30B's 20 GiB
//! `ram_floor_bytes` excludes it, so a test that varied real RAM would silently
//! be asserting about `qwen2.5-coder-3b`. Holding the model fixed and varying
//! only [`WindowFitInputs::admissible_bytes`] is what makes the four cases
//! comparable.
//!
//! ## The decision, in order
//!
//! ```text
//! fit_window(inputs):
//!   if config_n_ctx > n_ctx_train        → AboveTrained   (BR-2: no scaling)
//!
//!   if config_n_ctx is set:
//!     # The user named a window. There is no step-down: it fits or it is
//!     # refused. An explicit window waives the quarter-window rule (asking
//!     # for a SMALL window is honoured), never the memory check (BR-4).
//!     fits at admissible, or allow_over_memory  → Fits(ConfigOverride)
//!     else                                      → Refused
//!
//!   else:
//!     trained window at f16   fits → Fits(TrainedWindow)
//!     trained window at q8_0  fits → Fits(MemoryFit)
//!     else step down at q8_0 to the largest multiple of WINDOW_STEP that fits
//!       result >= n_ctx_train / 4 → Fits(MemoryFit)
//!       result <  n_ctx_train / 4 → Refused          (BR-4)
//! ```
//!
//! ## Two waivers, deliberately separate (BR-4, ADR-616-7)
//!
//! `[inference] n_ctx` and `allow_over_memory` answer different questions and
//! neither implies the other:
//!
//! | key | waives | does not waive |
//! |---|---|---|
//! | `n_ctx` | the quarter-window refusal | the memory check |
//! | `allow_over_memory` | the memory check | nothing else |
//!
//! Collapsing them would let a user who asked for a *smaller* window silently
//! get an overcommitted load, which is the opposite of what they asked for.

use crate::probe::GIB;

/// The KV cache element type the context is allocated with.
///
/// Teton's own enum rather than `llama_cpp_2`'s, so this module — and every test
/// over it — compiles in builds without the `llama` feature, which is all of CI.
/// The feature-gated loader maps it across at the FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    /// 16-bit floats — the llama.cpp default, and what the measurement in
    /// [`MEASURED_KV_BYTES_PER_TOKEN_F16`] was taken at.
    F16,
    /// 8-bit quantized — half the bytes of [`Self::F16`], per element.
    Q8_0,
}

impl KvCacheType {
    /// Bytes per stored element.
    #[must_use]
    pub const fn bytes_per_element(self) -> u64 {
        match self {
            Self::F16 => 2,
            Self::Q8_0 => 1,
        }
    }

    /// The wire/TOML spelling, for events and `model-selection.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8_0 => "q8_0",
        }
    }

    /// Parse a user-authored spelling — `[inference] kv_cache_type`.
    ///
    /// This is the **one** authority on the spellings. `teton-core`'s config
    /// carries the key as a `String` rather than mirroring this enum, because
    /// `teton-inference` is a deliberate leaf (serde/toml/thiserror only, no
    /// edge to `teton-core` or `teton-protocol`) and a mirrored enum in another
    /// crate is a second home that drifts. The same shape as
    /// `LocalModelConfig::pinned`, which carries a catalog model *name* rather
    /// than a catalog type, and is validated where it is used.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "f16" => Some(Self::F16),
            "q8_0" => Some(Self::Q8_0),
            _ => None,
        }
    }

    /// Every spelling this type accepts, for a refusal message that lists them.
    pub const ALL: [Self; 2] = [Self::F16, Self::Q8_0];
}

/// The share of physical RAM the daemon may plan to occupy: **75 %**.
///
/// # Why a new number, and why this one (REQ-616 OQ-1, ADR-616-4)
///
/// OQ-1 asked whether the existing model-selection rule already carries a
/// headroom figure to reuse. It does not.
/// [`crate::catalog::ModelEntry::ram_floor_bytes`] is a per-model **minimum-RAM
/// gate** — "is this machine big enough for this model at all" — not a budget of
/// bytes the process may spend. The two answer different questions, and the
/// floor is in fact already slightly inconsistent with the KV measurement: the
/// 30B's 20 GiB floor less its 17.3 GiB of weights leaves 2.7 GiB, against the
/// 3.0 GiB the KV cache measures at the *current* 32,768 window.
///
/// So the fraction is stated here rather than borrowed. REQ-616 AC-5 bounds it:
/// a 48 GiB machine must **admit** q8_0 at the trained window (30.3 GiB
/// resident) and **refuse** f16 (42.3 GiB), which puts the fraction in
/// `[62.5 %, 87.5 %)`. 75 % is the midpoint, and on the 48 GiB dogfood machine
/// it leaves 12 GiB for the user's own work — which is the promise
/// `ram_floor_bytes`'s own doc makes ("never degrade the machine").
///
/// `ram_floor_bytes` is deliberately **not** adjusted here: changing it changes
/// model *selection*, which REQ-616 puts out of scope.
pub const ADMISSIBLE_RAM_PERCENT: u64 = 75;

/// The bytes of `physical` RAM the daemon may plan to occupy.
///
/// Multiplies **before** dividing. The other order (`physical / 100 * percent`)
/// silently discards up to 99 × percent bytes — on a 48 GiB machine it lands 39
/// bytes below `36 GiB`, which is harmless in itself but makes every figure
/// derived from it un-checkable against a round number. Overflow is not
/// reachable: `u64::MAX / 100` is ~184 petabytes of RAM.
#[must_use]
pub const fn admissible_bytes(physical_bytes: u64) -> u64 {
    physical_bytes / 100 * ADMISSIBLE_RAM_PERCENT
        + physical_bytes % 100 * ADMISSIBLE_RAM_PERCENT / 100
}

/// A stated allowance for llama.cpp's compute buffers: **1 GiB**.
///
/// An allowance, not a measurement, and named so rather than folded into a fudge
/// factor on the KV figure. It is deliberately coarse because none of AC-5's
/// four cases turn on it: at 48 GiB the f16 estimate is refused and the q8_0
/// estimate admitted for any allowance from 0 to 2 GiB, and the 16 / 32 / 96 GiB
/// cases have margins wider still. Should a future model make it load-bearing,
/// it wants measuring rather than nudging.
pub const COMPUTE_BUFFER_BYTES: u64 = GIB;

/// The granularity a stepped-down window is rounded to: 4,096 tokens (BR-3).
pub const WINDOW_STEP: u32 = 4_096;

/// KV bytes per token at f16 for `qwen3-coder-30b-a3b`, **measured**: the daemon
/// reported a 3,072 MiB cache at a 32,768-token window, which is 98,304 B/token.
///
/// Used as the fallback when GGUF metadata is unavailable — and when it is used,
/// the fallback is named in `local_window_decided` rather than applied silently
/// (LESSON-456: the daemon knew, the message must say).
///
/// [`kv_bytes_per_token`] must reproduce this figure from the model's own
/// metadata; `metadata_derivation_matches_the_measurement` pins that in both
/// directions, so a wrong formula cannot hide behind a right constant.
pub const MEASURED_KV_BYTES_PER_TOKEN_F16: u64 = 98_304;

/// KV bytes per token from the model's shape.
///
/// `2 (one K and one V) × n_layer × n_head_kv × head_dim × bytes_per_element`.
#[must_use]
pub const fn kv_bytes_per_token(
    n_layer: u64,
    n_head_kv: u64,
    head_dim: u64,
    kv: KvCacheType,
) -> u64 {
    2 * n_layer * n_head_kv * head_dim * kv.bytes_per_element()
}

/// Why the window came out where it did — the three values REQ-616 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowReason {
    /// The model's trained window fit at f16, with nothing to trade.
    TrainedWindow,
    /// Memory decided something: a quantized cache, a stepped-down window, or
    /// both. The event carries the figures that forced it.
    MemoryFit,
    /// The user's `[inference]` table decided it — an explicit `n_ctx`, or
    /// `allow_over_memory` loading past the admissible share.
    ///
    /// `allow_over_memory` reports here rather than under a fourth value
    /// because REQ-616 fixes this enum at three; the event's
    /// `resident_bytes_estimate` beside its `admissible_bytes` is what shows a
    /// reader that the memory check was waived, and it shows it as arithmetic
    /// rather than as a label.
    ConfigOverride,
}

/// Everything [`fit_window`] needs, and nothing it could detect for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFitInputs {
    /// The model's trained window, read from GGUF metadata. The hard ceiling:
    /// no scaling is applied above it (BR-2).
    pub n_ctx_train: u32,
    /// Resident size of the weights.
    pub weights_bytes: u64,
    /// What this machine may spend — [`admissible_bytes`] of physical RAM.
    pub admissible_bytes: u64,
    /// KV bytes per token **at f16**; q8_0 is derived as half.
    pub kv_bytes_per_token_f16: u64,
    /// `[inference] n_ctx`, when the user set one.
    pub config_n_ctx: Option<u32>,
    /// `[inference] kv_cache_type`, when the user set one. Pins the type the
    /// probe would otherwise choose.
    pub config_kv: Option<KvCacheType>,
    /// `[inference] allow_over_memory`. Waives the memory check and nothing else.
    pub allow_over_memory: bool,
}

/// What the probe decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowDecision {
    /// Load at this window and KV type.
    Fits {
        /// The window to allocate.
        n_ctx: u32,
        /// The KV cache element type to allocate it at.
        kv: KvCacheType,
        /// Weights + KV at `n_ctx` + [`COMPUTE_BUFFER_BYTES`].
        resident_bytes: u64,
        /// Which rule produced this window.
        reason: WindowReason,
    },
    /// The machine cannot hold a window worth loading, and no override says to
    /// try anyway (BR-4). Carries the arithmetic the message must state.
    Refused {
        /// The smallest window that would have been accepted — `n_ctx_train / 4`
        /// rounded down to [`WINDOW_STEP`] — or the user's `n_ctx` when they set
        /// one, since then that is the only window in question.
        wanted_n_ctx: u32,
        /// Resident bytes at `wanted_n_ctx`, at the cheapest KV type available.
        resident_bytes: u64,
        /// What the machine allowed.
        admissible_bytes: u64,
        /// `resident_bytes - admissible_bytes`: how much more is needed.
        shortfall_bytes: u64,
    },
    /// `[inference] n_ctx` asked for more than the model was trained on (BR-2).
    /// No RoPE or YaRN scaling is applied, so this is refused rather than
    /// approximated.
    AboveTrained {
        /// What the config asked for.
        requested: u32,
        /// What the model actually supports.
        n_ctx_train: u32,
    },
}

/// Resident bytes for a window at a KV type.
#[must_use]
const fn resident_at(inputs: &WindowFitInputs, n_ctx: u32, kv: KvCacheType) -> u64 {
    let per_token = match kv {
        KvCacheType::F16 => inputs.kv_bytes_per_token_f16,
        KvCacheType::Q8_0 => inputs.kv_bytes_per_token_f16 / 2,
    };
    inputs
        .weights_bytes
        .saturating_add(per_token.saturating_mul(n_ctx as u64))
        .saturating_add(COMPUTE_BUFFER_BYTES)
}

/// Round `n` down to a multiple of [`WINDOW_STEP`].
#[must_use]
const fn floor_to_step(n: u32) -> u32 {
    n / WINDOW_STEP * WINDOW_STEP
}

/// The smallest window that will be accepted without an explicit `n_ctx`:
/// one quarter of the trained window, on the step grid (BR-4).
#[must_use]
pub const fn minimum_unconfigured_window(n_ctx_train: u32) -> u32 {
    floor_to_step(n_ctx_train / 4)
}

/// Decide the local engine's window and KV cache type.
///
/// See the module docs for the ordering and for why the two config waivers stay
/// separate.
#[must_use]
pub fn fit_window(inputs: &WindowFitInputs) -> WindowDecision {
    // BR-2: never above the trained window. Checked before anything else, so a
    // config that asks for the impossible is told that rather than being
    // silently clamped into a decision it did not ask for.
    if let Some(requested) = inputs.config_n_ctx {
        if requested > inputs.n_ctx_train {
            return WindowDecision::AboveTrained {
                requested,
                n_ctx_train: inputs.n_ctx_train,
            };
        }
    }

    // The KV types to try, cheapest-quality first. A configured type pins the
    // list to one entry: the user chose, so the probe does not second-guess.
    let candidates: &[KvCacheType] = match inputs.config_kv {
        Some(KvCacheType::F16) => &[KvCacheType::F16],
        Some(KvCacheType::Q8_0) => &[KvCacheType::Q8_0],
        None => &[KvCacheType::F16, KvCacheType::Q8_0],
    };

    if let Some(requested) = inputs.config_n_ctx {
        // The user named a window. It fits or it is refused — there is no
        // step-down here, because stepping down would hand them a window they
        // did not ask for (BR-4, AC-5's 16 GiB case).
        for &kv in candidates {
            let resident = resident_at(inputs, requested, kv);
            if resident <= inputs.admissible_bytes || inputs.allow_over_memory {
                return WindowDecision::Fits {
                    n_ctx: requested,
                    kv,
                    resident_bytes: resident,
                    reason: WindowReason::ConfigOverride,
                };
            }
        }
        let cheapest = *candidates.last().unwrap_or(&KvCacheType::Q8_0);
        let resident = resident_at(inputs, requested, cheapest);
        return WindowDecision::Refused {
            wanted_n_ctx: requested,
            resident_bytes: resident,
            admissible_bytes: inputs.admissible_bytes,
            shortfall_bytes: resident.saturating_sub(inputs.admissible_bytes),
        };
    }

    // No configured window: the trained window is what we want (BR-1).
    for &kv in candidates {
        let resident = resident_at(inputs, inputs.n_ctx_train, kv);
        if resident <= inputs.admissible_bytes {
            return WindowDecision::Fits {
                n_ctx: inputs.n_ctx_train,
                kv,
                resident_bytes: resident,
                // f16 at the trained window is the untraded case; reaching q8_0
                // means memory chose the type, which is what `memory_fit` says.
                reason: if kv == KvCacheType::F16 {
                    WindowReason::TrainedWindow
                } else {
                    WindowReason::MemoryFit
                },
            };
        }
    }

    if inputs.allow_over_memory {
        // The user said to load anyway. Honour the trained window at the
        // cheapest type on offer, and let the event's arithmetic show that
        // resident exceeds admissible.
        let kv = *candidates.last().unwrap_or(&KvCacheType::Q8_0);
        return WindowDecision::Fits {
            n_ctx: inputs.n_ctx_train,
            kv,
            resident_bytes: resident_at(inputs, inputs.n_ctx_train, kv),
            reason: WindowReason::ConfigOverride,
        };
    }

    // Step the window down at the cheapest type until it fits (BR-3). Closed
    // form rather than a loop: the remaining budget divided by the per-token
    // cost, floored to the step grid.
    let kv = *candidates.last().unwrap_or(&KvCacheType::Q8_0);
    let per_token = match kv {
        KvCacheType::F16 => inputs.kv_bytes_per_token_f16,
        KvCacheType::Q8_0 => inputs.kv_bytes_per_token_f16 / 2,
    };
    let fixed = inputs.weights_bytes.saturating_add(COMPUTE_BUFFER_BYTES);
    let for_kv = inputs.admissible_bytes.saturating_sub(fixed);
    // `checked_div` rather than a guarded `/`: a zero per-token cost is a
    // degenerate model shape, and the trained window is the honest answer for it.
    let raw = for_kv
        .checked_div(per_token)
        .map_or(inputs.n_ctx_train, |t| u32::try_from(t).unwrap_or(u32::MAX));
    let stepped = floor_to_step(raw.min(inputs.n_ctx_train));
    let minimum = minimum_unconfigured_window(inputs.n_ctx_train);

    if stepped >= minimum && stepped > 0 {
        return WindowDecision::Fits {
            n_ctx: stepped,
            kv,
            resident_bytes: resident_at(inputs, stepped, kv),
            reason: WindowReason::MemoryFit,
        };
    }

    // Below the floor: refuse, and report against the floor rather than against
    // whatever tiny window the arithmetic produced, because the floor is the
    // number the remedy has to clear.
    let resident = resident_at(inputs, minimum, kv);
    WindowDecision::Refused {
        wanted_n_ctx: minimum,
        resident_bytes: resident,
        admissible_bytes: inputs.admissible_bytes,
        shortfall_bytes: resident.saturating_sub(inputs.admissible_bytes),
    }
}

#[cfg(test)]
mod tests {
    //! ## Mutations run against this module (conventions.md: show the test can
    //! fail before trusting that it passed)
    //!
    //! | mutation | goes red |
    //! |---|---|
    //! | [`ADMISSIBLE_RAM_PERCENT`] 75 → 90 | `fit_window_table_at_four_ram_figures`, `kv_type_steps_f16_then_q8_then_window`, `admissible_fraction_is_inside_the_band_ac5_implies` |
    //! | q8_0 no longer half of f16 in `resident_at` | `fit_window_table_at_four_ram_figures`, `admissible_fraction_is_inside_the_band_ac5_implies` |
    //! | the configured-`n_ctx` arm waives the memory check | `shortfall_refuses_and_waivers_are_independent` |
    //!
    //! The third is the one worth keeping: collapsing the two waivers is the
    //! defect ADR-616-7 exists to prevent, and exactly one test catches it.

    use super::*;

    /// `qwen3-coder-30b-a3b` as the catalog carries it: 18,556,689,568 bytes of
    /// weights (17.28 GiB), trained to 262,144 tokens.
    const WEIGHTS_30B: u64 = 18_556_689_568;
    const TRAINED: u32 = 262_144;

    fn inputs(physical_gib: u64) -> WindowFitInputs {
        WindowFitInputs {
            n_ctx_train: TRAINED,
            weights_bytes: WEIGHTS_30B,
            admissible_bytes: admissible_bytes(physical_gib * GIB),
            kv_bytes_per_token_f16: MEASURED_KV_BYTES_PER_TOKEN_F16,
            config_n_ctx: None,
            config_kv: None,
            allow_over_memory: false,
        }
    }

    /// The metadata formula must reproduce the measurement, or one of them is
    /// wrong and the estimate is built on it.
    ///
    /// Qwen3-Coder-30B-A3B: 48 layers, 4 KV heads, head_dim 128.
    /// `2 × 48 × 4 × 128 × 2 = 98,304` — the measured 3,072 MiB at 32,768.
    ///
    /// Mutation: change any factor and the equality fails; change
    /// [`MEASURED_KV_BYTES_PER_TOKEN_F16`] and it fails from the other side.
    #[test]
    fn metadata_derivation_matches_the_measurement() {
        let derived = kv_bytes_per_token(48, 4, 128, KvCacheType::F16);
        assert_eq!(
            derived, MEASURED_KV_BYTES_PER_TOKEN_F16,
            "the shape-derived KV cost and the measured one must agree"
        );
        // And the measurement's own provenance: 3,072 MiB at 32,768 tokens.
        assert_eq!(derived * 32_768, 3_072 * 1024 * 1024);
        // q8_0 is half of f16, per element.
        assert_eq!(
            kv_bytes_per_token(48, 4, 128, KvCacheType::Q8_0),
            derived / 2
        );
    }

    /// AC-5's four cases, with the model held fixed and only admissible RAM
    /// varying — the pure-function shape ADR-616-3 requires.
    #[test]
    fn fit_window_table_at_four_ram_figures() {
        // 48 GiB: f16 (42.3 GiB) exceeds the 36 GiB share; q8_0 (30.3) fits.
        match fit_window(&inputs(48)) {
            WindowDecision::Fits {
                n_ctx,
                kv,
                reason,
                resident_bytes,
            } => {
                assert_eq!(n_ctx, TRAINED);
                assert_eq!(kv, KvCacheType::Q8_0);
                assert_eq!(reason, WindowReason::MemoryFit);
                assert!(resident_bytes <= admissible_bytes(48 * GIB));
                // and f16 genuinely would not have fit — the premise of the case
                assert!(
                    resident_at(&inputs(48), TRAINED, KvCacheType::F16)
                        > admissible_bytes(48 * GIB)
                );
            }
            other => panic!("48 GiB should fit at q8_0, got {other:?}"),
        }

        // 96 GiB: f16 fits outright, nothing traded.
        match fit_window(&inputs(96)) {
            WindowDecision::Fits {
                n_ctx, kv, reason, ..
            } => {
                assert_eq!(n_ctx, TRAINED);
                assert_eq!(kv, KvCacheType::F16);
                assert_eq!(reason, WindowReason::TrainedWindow);
            }
            other => panic!("96 GiB should fit at f16, got {other:?}"),
        }

        // 16 GiB: the weights alone (17.28 GiB) exceed the 12 GiB share, so no
        // window fits and none is offered.
        match fit_window(&inputs(16)) {
            WindowDecision::Refused {
                shortfall_bytes,
                admissible_bytes: adm,
                ..
            } => {
                assert!(shortfall_bytes > 0);
                assert_eq!(adm, admissible_bytes(16 * GIB));
                assert!(WEIGHTS_30B > adm, "the premise: weights alone overrun");
            }
            other => panic!("16 GiB should refuse, got {other:?}"),
        }

        // 32 GiB: q8_0 at the trained window (30.3) exceeds the 24 GiB share, so
        // the window steps down — and lands above the 65,536 floor, so it loads.
        match fit_window(&inputs(32)) {
            WindowDecision::Fits {
                n_ctx, kv, reason, ..
            } => {
                assert_eq!(kv, KvCacheType::Q8_0);
                assert_eq!(reason, WindowReason::MemoryFit);
                assert!(n_ctx < TRAINED, "32 GiB cannot hold the trained window");
                assert!(n_ctx >= minimum_unconfigured_window(TRAINED));
                assert_eq!(n_ctx % WINDOW_STEP, 0, "stepped to the 4,096 grid");
            }
            other => panic!("32 GiB should step down, got {other:?}"),
        }
    }

    /// BR-2: no scaling above the trained window, and the benign path — a value
    /// at or below it is accepted rather than swept up in the same refusal.
    #[test]
    fn config_n_ctx_above_trained_is_rejected() {
        let mut over = inputs(96);
        over.config_n_ctx = Some(300_000);
        assert_eq!(
            fit_window(&over),
            WindowDecision::AboveTrained {
                requested: 300_000,
                n_ctx_train: TRAINED,
            }
        );

        // Benign path: exactly the trained window is not "above" it.
        let mut at = inputs(96);
        at.config_n_ctx = Some(TRAINED);
        assert!(matches!(fit_window(&at), WindowDecision::Fits { .. }));

        // Benign path: a smaller window is honoured, not refused for being small.
        let mut under = inputs(96);
        under.config_n_ctx = Some(65_536);
        match fit_window(&under) {
            WindowDecision::Fits { n_ctx, reason, .. } => {
                assert_eq!(n_ctx, 65_536);
                assert_eq!(reason, WindowReason::ConfigOverride);
            }
            other => panic!("a smaller configured window should load, got {other:?}"),
        }
    }

    /// BR-3: the type ladder is f16 → q8_0 → step the window, in that order, and
    /// a configured type pins it.
    #[test]
    fn kv_type_steps_f16_then_q8_then_window() {
        // Ladder rung 1 and 2 are covered by the 96/48 GiB cases above; this
        // pins that a *configured* type is not overridden by the probe.
        let mut pinned = inputs(48);
        pinned.config_kv = Some(KvCacheType::F16);
        // f16 does not fit at 48 GiB, and the probe may not silently fall to
        // q8_0 when the user pinned f16 — it steps the window instead.
        match fit_window(&pinned) {
            WindowDecision::Fits { kv, n_ctx, .. } => {
                assert_eq!(kv, KvCacheType::F16, "a pinned type is not second-guessed");
                assert!(n_ctx < TRAINED);
            }
            WindowDecision::Refused { .. } => {}
            other => panic!("unexpected {other:?}"),
        }

        // And the step grid is respected wherever a step happens.
        if let WindowDecision::Fits { n_ctx, .. } = fit_window(&inputs(32)) {
            assert_eq!(n_ctx % WINDOW_STEP, 0);
        }
    }

    /// BR-4: a shortfall refuses, and the two waivers are independent — neither
    /// one does the other's job.
    #[test]
    fn shortfall_refuses_and_waivers_are_independent() {
        // Baseline: 16 GiB refuses.
        assert!(matches!(
            fit_window(&inputs(16)),
            WindowDecision::Refused { .. }
        ));

        // An explicit n_ctx does NOT rescue it — it waives the quarter-window
        // rule, not the memory check (AC-5's 16 GiB case).
        let mut explicit = inputs(16);
        explicit.config_n_ctx = Some(65_536);
        assert!(
            matches!(fit_window(&explicit), WindowDecision::Refused { .. }),
            "an explicit window must not waive the memory check"
        );

        // allow_over_memory alone DOES load it, at the trained window.
        let mut over = inputs(16);
        over.allow_over_memory = true;
        match fit_window(&over) {
            WindowDecision::Fits {
                n_ctx,
                reason,
                resident_bytes,
                ..
            } => {
                assert_eq!(n_ctx, TRAINED);
                assert_eq!(reason, WindowReason::ConfigOverride);
                assert!(
                    resident_bytes > admissible_bytes(16 * GIB),
                    "the event's arithmetic is what shows the check was waived"
                );
            }
            other => panic!("allow_over_memory should load, got {other:?}"),
        }

        // Both together: the user's window, loaded over memory.
        let mut both = inputs(16);
        both.config_n_ctx = Some(65_536);
        both.allow_over_memory = true;
        match fit_window(&both) {
            WindowDecision::Fits { n_ctx, reason, .. } => {
                assert_eq!(n_ctx, 65_536);
                assert_eq!(reason, WindowReason::ConfigOverride);
            }
            other => panic!("both waivers should load at the named window, got {other:?}"),
        }

        // Benign path: a machine with room refuses nothing and needs no waiver.
        assert!(matches!(
            fit_window(&inputs(96)),
            WindowDecision::Fits { .. }
        ));
    }

    /// The refusal carries the arithmetic the message has to state (BR-4), and
    /// reports it against the floor the remedy must clear.
    #[test]
    fn refusal_reports_against_the_quarter_window_floor() {
        match fit_window(&inputs(16)) {
            WindowDecision::Refused {
                wanted_n_ctx,
                resident_bytes,
                admissible_bytes: adm,
                shortfall_bytes,
            } => {
                assert_eq!(wanted_n_ctx, 65_536, "one quarter of 262,144");
                assert_eq!(shortfall_bytes, resident_bytes - adm);
                assert_eq!(adm, admissible_bytes(16 * GIB));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The admissible fraction sits inside the band AC-5 implies, and the
    /// assertion is written as the band rather than as the number so that
    /// moving the constant out of range fails here rather than in a distant
    /// table.
    /// The fraction is exact on a round machine: 75 % of 48 GiB is 36 GiB, to
    /// the byte.
    ///
    /// Mutation: revert to `physical / 100 * percent` and this fails by 39
    /// bytes — the slip that made every derived figure un-checkable against a
    /// round number.
    #[test]
    fn admissible_bytes_is_exact_on_round_machines() {
        assert_eq!(admissible_bytes(48 * GIB), 36 * GIB);
        assert_eq!(admissible_bytes(96 * GIB), 72 * GIB);
        assert_eq!(admissible_bytes(32 * GIB), 24 * GIB);
        assert_eq!(admissible_bytes(16 * GIB), 12 * GIB);
        assert_eq!(admissible_bytes(0), 0);
    }

    #[test]
    fn admissible_fraction_is_inside_the_band_ac5_implies() {
        let physical = 48 * GIB;
        let adm = admissible_bytes(physical);
        let q8 = resident_at(&inputs(48), TRAINED, KvCacheType::Q8_0);
        let f16 = resident_at(&inputs(48), TRAINED, KvCacheType::F16);
        assert!(q8 <= adm, "48 GiB must admit q8_0 at the trained window");
        assert!(f16 > adm, "48 GiB must refuse f16 at the trained window");
        assert!((62..88).contains(&ADMISSIBLE_RAM_PERCENT));
    }

    /// `as_str` and `parse` are inverses, and `ALL` enumerates every variant —
    /// so a third KV type cannot be added without this failing.
    ///
    /// Mutation: drop a variant from `ALL`, or change one spelling on one side.
    #[test]
    fn kv_spellings_round_trip_and_all_is_exhaustive() {
        for kv in KvCacheType::ALL {
            assert_eq!(KvCacheType::parse(kv.as_str()), Some(kv));
        }
        assert_eq!(KvCacheType::parse("bf16"), None);
        assert_eq!(KvCacheType::parse(""), None);
        // Exhaustiveness: a new variant makes this match fail to compile, and
        // the count assertion fails if it is not added to ALL.
        let counted = [KvCacheType::F16, KvCacheType::Q8_0]
            .iter()
            .filter(|k| KvCacheType::ALL.contains(k))
            .count();
        assert_eq!(counted, KvCacheType::ALL.len());
    }

    /// The window grid is a floor, never a round-up: a rounded-up window would
    /// overrun the very budget the step-down exists to respect.
    #[test]
    fn stepping_rounds_down_never_up() {
        for n in [0u32, 1, WINDOW_STEP - 1, WINDOW_STEP, WINDOW_STEP + 1] {
            assert!(floor_to_step(n) <= n);
            assert_eq!(floor_to_step(n) % WINDOW_STEP, 0);
        }
    }
}

/// Progress reporting for a long prefill (REQ-616 BR-9, AC-9).
///
/// A cold 262,144-token prefill on Apple Silicon runs for a minute or two, and
/// a turn that prints nothing for two minutes is indistinguishable from a hung
/// one. This decides *when* to say something; the caller does the saying.
///
/// **Pure, and outside the `llama` feature gate**, like [`crate::window`]: the
/// prefill loop that drives it is FFI and never compiles in CI, so a policy
/// living inside that loop would be a policy no test could reach. The clock is
/// a parameter for the same reason — a cadence asserted against a real
/// `Instant::now()` is a flaky test, and one asserted against a supplied one is
/// a claim about the rule.
pub mod prefill {
    use std::time::{Duration, Instant};

    /// Below this many prompt tokens, a prefill reports nothing.
    ///
    /// It is the *old* window, and deliberately: a prefill that would have fit
    /// in the pre-REQ-616 engine is not a long one, and reporting on it would
    /// turn every ordinary turn into a progress bar. Above it, the prefill is
    /// doing something the previous release could not do at all, which is
    /// exactly when a user needs telling that it is working.
    pub const PREFILL_PROGRESS_THRESHOLD_TOKENS: u32 = 32_768;

    /// At most one report per second (the spec's "at most once per second").
    pub const PREFILL_PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(1);

    /// One progress report.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct PrefillProgress {
        /// Prompt tokens decoded so far.
        pub tokens_done: u32,
        /// Prompt tokens in total.
        pub tokens_total: u32,
        /// Throughput since the prefill started.
        pub tokens_per_second: f32,
    }

    /// Decides whether this moment in a prefill earns a report.
    ///
    /// Rate-limited rather than sampled every N tokens: what a user is waiting
    /// on is *wall-clock*, and a token-count cadence produces a burst on a fast
    /// machine and silence on a slow one — the two cases where the cadence
    /// matters most, and it gets both backwards.
    #[derive(Debug)]
    pub struct PrefillReporter {
        total: u32,
        started: Instant,
        last: Option<Instant>,
        emitted: u32,
    }

    impl PrefillReporter {
        /// A reporter for a prefill of `total` prompt tokens, started at
        /// `started`.
        #[must_use]
        pub fn new(total: u32, started: Instant) -> Self {
            Self {
                total,
                started,
                last: None,
                emitted: 0,
            }
        }

        /// Whether this prefill reports at all.
        #[must_use]
        pub fn reports(&self) -> bool {
            self.total > PREFILL_PROGRESS_THRESHOLD_TOKENS
        }

        /// How many reports have been emitted.
        #[must_use]
        pub fn emitted(&self) -> u32 {
            self.emitted
        }

        /// The report this moment earns, if any.
        ///
        /// The **first** report is not rate-limited against `started`: a long
        /// prefill should say something as soon as it knows it is long, rather
        /// than staying silent for the first second of a two-minute wait.
        /// Subsequent ones are spaced by [`PREFILL_PROGRESS_MIN_INTERVAL`].
        pub fn tick(&mut self, tokens_done: u32, now: Instant) -> Option<PrefillProgress> {
            if !self.reports() {
                return None;
            }
            if let Some(last) = self.last {
                if now.duration_since(last) < PREFILL_PROGRESS_MIN_INTERVAL {
                    return None;
                }
            }
            self.last = Some(now);
            self.emitted += 1;
            let elapsed = now.duration_since(self.started).as_secs_f32();
            Some(PrefillProgress {
                tokens_done,
                tokens_total: self.total,
                // A zero elapsed would divide by zero; the honest throughput
                // before any time has passed is unknown, and 0.0 says "not yet
                // measurable" rather than reporting an infinity.
                tokens_per_second: if elapsed > 0.0 {
                    tokens_done as f32 / elapsed
                } else {
                    0.0
                },
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// AC-9: a 100,000-token prefill reports at least once.
        ///
        /// Mutation: raise the threshold above 100,000 and this fails.
        #[test]
        fn a_long_prefill_reports_at_least_once() {
            let t0 = Instant::now();
            let mut r = PrefillReporter::new(100_000, t0);
            assert!(r.reports());
            assert!(r.tick(1_000, t0).is_some(), "the first report is prompt");
            assert_eq!(r.emitted(), 1);
        }

        /// The benign path, and the one that keeps ordinary turns quiet: a
        /// prefill at or below the old window reports nothing at all.
        ///
        /// A detector validated only where it fires ships broken (LESSON-440),
        /// and here "fires on every turn" is the failure users would actually
        /// notice.
        #[test]
        fn a_short_prefill_reports_nothing() {
            let t0 = Instant::now();
            for total in [0u32, 1, 4_096, PREFILL_PROGRESS_THRESHOLD_TOKENS] {
                let mut r = PrefillReporter::new(total, t0);
                assert!(!r.reports(), "{total} must not report");
                assert!(r.tick(total, t0 + Duration::from_secs(10)).is_none());
                assert_eq!(r.emitted(), 0);
            }
            // One token past the threshold does report — the boundary is
            // asserted from both sides so an off-by-one cannot hide.
            let over = PrefillReporter::new(PREFILL_PROGRESS_THRESHOLD_TOKENS + 1, t0);
            assert!(over.reports());
        }

        /// The cadence is a real bound: a fast prefill does not produce a burst.
        ///
        /// This is the assertion the spec's "at most once per second" earns.
        /// Mutation: drop the `last` check and 100 ticks produce 100 reports.
        #[test]
        fn a_fast_prefill_does_not_burst() {
            let t0 = Instant::now();
            let mut r = PrefillReporter::new(262_144, t0);
            // A hundred ticks inside one second.
            for i in 0..100u32 {
                r.tick(i * 100, t0 + Duration::from_millis(u64::from(i) * 9));
            }
            assert_eq!(
                r.emitted(),
                1,
                "a hundred ticks inside one second must produce one report"
            );

            // Crossing the interval earns exactly one more.
            r.tick(50_000, t0 + Duration::from_millis(1_500));
            assert_eq!(r.emitted(), 2);
            r.tick(60_000, t0 + Duration::from_millis(1_900));
            assert_eq!(r.emitted(), 2, "still inside the interval");
            r.tick(70_000, t0 + Duration::from_millis(2_600));
            assert_eq!(r.emitted(), 3);
        }

        /// The payload carries what a waiting user needs, and the throughput is
        /// measured from the start rather than from the last report.
        #[test]
        fn the_report_carries_progress_and_throughput() {
            let t0 = Instant::now();
            let mut r = PrefillReporter::new(200_000, t0);
            let p = r
                .tick(50_000, t0 + Duration::from_secs(2))
                .expect("a long prefill reports");
            assert_eq!(p.tokens_done, 50_000);
            assert_eq!(p.tokens_total, 200_000);
            assert!(
                (p.tokens_per_second - 25_000.0).abs() < 1.0,
                "50,000 tokens in 2 s is 25,000/s, got {}",
                p.tokens_per_second
            );
        }

        /// Zero elapsed time reports zero throughput rather than an infinity.
        #[test]
        fn zero_elapsed_does_not_divide_by_zero() {
            let t0 = Instant::now();
            let mut r = PrefillReporter::new(100_000, t0);
            let p = r.tick(0, t0).expect("reports");
            assert!(p.tokens_per_second.is_finite());
            assert_eq!(p.tokens_per_second, 0.0);
        }
    }
}
