//! The prefix-cache *decision* (REQ-564): may this turn reuse the resident KV,
//! and how much of it?
//!
//! This module is deliberately **not** behind the `llama` feature. The reuse
//! rule and the miss taxonomy are the subtlest logic in REQ-564, and the real
//! engine lives behind a non-default feature that CI never compiles — so a
//! design that kept the policy next to the FFI would ship its most delicate
//! code with zero automated coverage. Here it is a pure function over token
//! ids, unit-tested in every default build, and the gated module is left
//! holding only llama.cpp calls (architecture D-2).
//!
//! Token ids are plain `i32` rather than `LlamaToken` for the same reason: the
//! policy must not depend on the binding. The engine converts at its boundary.

/// Why a turn could not reuse the resident prefix.
///
/// Every miss carries its *actual* reason (BR-8). A divergence is not an error
/// and an error is never dressed up as a miss — [`PrefixCacheState::probe`]
/// cannot fail, so this enum describes only legitimate cache outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    /// No prefix was resident — first turn on this engine.
    Cold,
    /// A prefix was resident, but for a different session (BR-3's single slot).
    SessionSwitch,
    /// Same session, but the token streams disagree — context compaction,
    /// truncation, or a template change rewrote history (BR-2).
    Divergent,
    /// The prefix was dropped by [`PrefixCacheState::evict`] before this turn.
    ///
    /// Distinguished from [`MissReason::Cold`] so an eviction is never
    /// reported as "we simply had nothing" (BR-4 reports evictions, it does not
    /// swallow them).
    Evicted,
}

impl MissReason {
    /// The wire spelling used by the `prefix_cache` event.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::SessionSwitch => "session_switch",
            Self::Divergent => "divergent",
            Self::Evicted => "evicted",
        }
    }
}

/// Why a resident prefix was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionReason {
    /// The runtime asked for the memory back.
    MemoryPressure,
    /// The engine is being unloaded or swapped for another model.
    EngineUnload,
    /// A generation failed partway, so the resident KV no longer provably
    /// matches the recorded prefix and cannot be trusted for reuse.
    GenerationFailed,
}

impl EvictionReason {
    /// The wire spelling used by the `prefix_cache` event.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemoryPressure => "memory_pressure",
            Self::EngineUnload => "engine_unload",
            Self::GenerationFailed => "generation_failed",
        }
    }
}

/// What a turn may do with the resident KV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
    /// Reuse the first `reuse` positions; prefill `tokens[reuse..]`.
    ///
    /// `reuse` is always `>= 1` and always `< tokens.len()`, so the suffix is
    /// never empty — see [`PrefixCacheState::probe`] for why that matters.
    Hit {
        /// Tokens whose KV is reused (the truncation point).
        reuse: usize,
    },
    /// Prefill everything from position zero.
    Miss(MissReason),
}

impl CacheDecision {
    /// Tokens reused — `0` on a miss.
    #[must_use]
    pub fn reused(self) -> usize {
        match self {
            Self::Hit { reuse } => reuse,
            Self::Miss(_) => 0,
        }
    }

    /// The miss reason, or `None` on a hit.
    #[must_use]
    pub fn miss_reason(self) -> Option<MissReason> {
        match self {
            Self::Hit { .. } => None,
            Self::Miss(reason) => Some(reason),
        }
    }
}

/// The single cache slot's bookkeeping (BR-3: one slot per loaded engine).
///
/// Holds no llama.cpp state — only the description of what the resident KV
/// contains. The gated engine keeps this in lockstep with the real context.
#[derive(Debug, Default)]
pub struct PrefixCacheState {
    /// The session the resident prefix belongs to, or `None` when nothing is
    /// resident.
    session: Option<String>,
    /// The tokens whose KV is resident, in order.
    tokens: Vec<i32>,
    /// Armed by [`Self::evict`], consumed by the next [`Self::probe`], so an
    /// evicted slot reports `Evicted` exactly once and `Cold` thereafter.
    evicted: bool,
}

impl PrefixCacheState {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session whose prefix is resident, if any.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// How many tokens are resident.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether nothing is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Decide what `tokens` may reuse. Pure, total, and infallible.
    ///
    /// The rule is **not** a naive `tokens.starts_with(resident)`. Let
    /// `reuse = min(resident.len(), tokens.len() - 1)`; it is a hit iff
    /// `reuse > 0`, the session matches, and the two agree on `tokens[..reuse]`.
    ///
    /// The `- 1` is load-bearing. Sampling reads logits, and logits exist only
    /// for a token actually decoded in the final batch. A prompt that exactly
    /// equals the resident prefix (a retry, or a turn whose whole delta was
    /// dropped) would otherwise reuse everything, decode an empty batch, and
    /// sample from stale logits. Leaving one token to prefill keeps the batch
    /// non-empty on every path.
    ///
    /// It also makes a *shorter* agreeing prompt safe: the KV is truncated to
    /// `reuse` and the surplus discarded. That is a rewind, not the "partial
    /// reuse past a divergence point" BR-2 forbids — the comparison is over a
    /// contiguous head, so it can never skip over a disagreement.
    #[must_use]
    pub fn probe(&mut self, session: &str, tokens: &[i32]) -> CacheDecision {
        let was_evicted = std::mem::take(&mut self.evicted);

        let Some(resident_session) = self.session.as_deref() else {
            return CacheDecision::Miss(if was_evicted {
                MissReason::Evicted
            } else {
                MissReason::Cold
            });
        };
        if resident_session != session {
            return CacheDecision::Miss(MissReason::SessionSwitch);
        }
        // `tokens.len() - 1` without underflow: an empty prompt has nothing to
        // reuse and nothing to prefill, so it is a miss like any other.
        let reuse = self.tokens.len().min(tokens.len().saturating_sub(1));
        if reuse == 0 {
            return CacheDecision::Miss(MissReason::Divergent);
        }
        if self.tokens[..reuse] == tokens[..reuse] {
            CacheDecision::Hit { reuse }
        } else {
            CacheDecision::Miss(MissReason::Divergent)
        }
    }

    /// Install `tokens` as the resident prefix for `session`.
    ///
    /// Called after a generation completes, with **every** token whose KV the
    /// context now holds — the prompt *plus* the tokens decoded during
    /// generation. Recording only the prompt would leave the description
    /// shorter than the real KV, and the next turn's reuse offset would be
    /// computed against the wrong baseline. That is a correctness bug, not a
    /// performance one.
    pub fn record(&mut self, session: &str, tokens: Vec<i32>) {
        self.session = Some(session.to_owned());
        self.tokens = tokens;
        self.evicted = false;
    }

    /// Drop the resident prefix and arm the [`MissReason::Evicted`] report.
    ///
    /// Idempotent: evicting an already-empty slot still arms the reason, which
    /// is what makes "the cache was taken away from you" distinguishable from
    /// "you never had one" on the next turn.
    pub fn evict(&mut self) {
        self.session = None;
        self.tokens.clear();
        self.evicted = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated(session: &str, tokens: &[i32]) -> PrefixCacheState {
        let mut state = PrefixCacheState::new();
        state.record(session, tokens.to_vec());
        state
    }

    #[test]
    fn an_empty_slot_is_a_cold_miss() {
        let mut state = PrefixCacheState::new();
        assert_eq!(
            state.probe("s1", &[1, 2, 3]),
            CacheDecision::Miss(MissReason::Cold)
        );
    }

    #[test]
    fn an_extending_prompt_reuses_the_whole_resident_prefix() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s1", &[1, 2, 3, 4, 5]),
            CacheDecision::Hit { reuse: 3 }
        );
    }

    /// The `-1` rule: an identical prompt still leaves one token to prefill, so
    /// the final batch is never empty and sampling never reads stale logits.
    #[test]
    fn an_identical_prompt_still_leaves_one_token_to_prefill() {
        let mut state = populated("s1", &[1, 2, 3]);
        let decision = state.probe("s1", &[1, 2, 3]);
        assert_eq!(decision, CacheDecision::Hit { reuse: 2 });
        assert!(decision.reused() < 3, "the suffix must never be empty");
    }

    /// A prompt shorter than the resident prefix is a rewind, not a divergence:
    /// the surplus KV is truncated away.
    #[test]
    fn a_shorter_agreeing_prompt_rewinds_rather_than_diverging() {
        let mut state = populated("s1", &[1, 2, 3, 4, 5]);
        assert_eq!(state.probe("s1", &[1, 2, 3]), CacheDecision::Hit { reuse: 2 });
    }

    #[test]
    fn a_divergent_prompt_misses_with_the_divergent_reason() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s1", &[1, 2, 9, 4]),
            CacheDecision::Miss(MissReason::Divergent)
        );
    }

    /// Divergence at position 0 is a miss, never a zero-length hit.
    #[test]
    fn divergence_at_the_first_token_is_a_miss() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s1", &[9, 2, 3]),
            CacheDecision::Miss(MissReason::Divergent)
        );
    }

    #[test]
    fn another_session_misses_with_session_switch() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s2", &[1, 2, 3, 4]),
            CacheDecision::Miss(MissReason::SessionSwitch)
        );
    }

    #[test]
    fn a_single_token_prompt_has_nothing_to_reuse() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s1", &[1]),
            CacheDecision::Miss(MissReason::Divergent)
        );
    }

    #[test]
    fn an_empty_prompt_is_a_miss() {
        let mut state = populated("s1", &[1, 2, 3]);
        assert_eq!(
            state.probe("s1", &[]),
            CacheDecision::Miss(MissReason::Divergent)
        );
    }

    /// Eviction is reported once, then the slot reads as cold — an eviction is
    /// never silently indistinguishable from having had nothing (BR-4/BR-8).
    #[test]
    fn eviction_is_reported_once_then_reads_as_cold() {
        let mut state = populated("s1", &[1, 2, 3]);
        state.evict();
        assert_eq!(
            state.probe("s1", &[1, 2, 3, 4]),
            CacheDecision::Miss(MissReason::Evicted)
        );
        assert_eq!(
            state.probe("s1", &[1, 2, 3, 4]),
            CacheDecision::Miss(MissReason::Cold)
        );
    }

    #[test]
    fn recording_after_an_eviction_restores_hits() {
        let mut state = populated("s1", &[1, 2, 3]);
        state.evict();
        state.record("s1", vec![1, 2, 3, 4]);
        assert_eq!(
            state.probe("s1", &[1, 2, 3, 4, 5]),
            CacheDecision::Hit { reuse: 4 }
        );
    }

    #[test]
    fn decision_accessors_agree_with_the_variant() {
        assert_eq!(CacheDecision::Hit { reuse: 7 }.reused(), 7);
        assert_eq!(CacheDecision::Hit { reuse: 7 }.miss_reason(), None);
        assert_eq!(CacheDecision::Miss(MissReason::Cold).reused(), 0);
        assert_eq!(
            CacheDecision::Miss(MissReason::Cold).miss_reason(),
            Some(MissReason::Cold)
        );
    }

    #[test]
    fn wire_spellings_are_stable() {
        assert_eq!(MissReason::Cold.as_str(), "cold");
        assert_eq!(MissReason::SessionSwitch.as_str(), "session_switch");
        assert_eq!(MissReason::Divergent.as_str(), "divergent");
        assert_eq!(MissReason::Evicted.as_str(), "evicted");
        assert_eq!(EvictionReason::MemoryPressure.as_str(), "memory_pressure");
        assert_eq!(EvictionReason::EngineUnload.as_str(), "engine_unload");
        assert_eq!(
            EvictionReason::GenerationFailed.as_str(),
            "generation_failed"
        );
    }
}
