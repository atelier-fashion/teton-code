//! Capability profiles and the BR-6 harness-degradation mapping.
//!
//! A provider advertises how well it tool-calls, whether it supports parallel
//! calls, and its context window. From that, [`CapabilityProfile::harness_profile`]
//! derives a [`HarnessProfile`]: providers with weak tool-calling get a reduced
//! harness (smaller tool set, shorter loops, mandatory verification) instead of
//! the full agent loop (BR-6). The [`ToolCallTier`] enum itself is owned by
//! `teton-core`; this crate reuses it rather than duplicating the vocabulary.

use teton_core::{EffortLadder, ProviderCapabilities, ReasoningShape, ToolCallTier};

/// Full-loop iteration budget for a reliable (`Native`) tool-caller.
const NATIVE_MAX_ITERATIONS: u32 = 25;
/// Reduced-loop iteration budget for a `Degraded` tool-caller (BR-6).
const DEGRADED_MAX_ITERATIONS: u32 = 5;
/// Reduced tool-set cap for a `Degraded` tool-caller (BR-6).
const DEGRADED_MAX_TOOLS: u32 = 5;

/// A provider's capability profile, consulted by the adapter layer and router.
/// Mirrors `teton-core`'s [`ProviderCapabilities`] with adapter-side behavior
/// attached ([`CapabilityProfile::harness_profile`]). The `Default` is the
/// strict/unknown baseline: `Native` tier (from `teton-core`), no parallel
/// calls, unknown context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilityProfile {
    /// How reliably the provider follows the tool-call protocol.
    pub tool_call_tier: ToolCallTier,
    /// Whether the provider can emit multiple tool calls in one turn.
    pub parallel_calls: bool,
    /// Maximum context window in tokens (`0` means unknown / unset).
    pub max_context: u32,
    /// A user ceiling on the context budget, in tokens, below the window
    /// (REQ-586 BR-5). `0` means no cap.
    ///
    /// The adapter does not read it — the router derives the budget before the
    /// request is built — it is carried here only so the projection back to
    /// [`ProviderCapabilities`] stays lossless, for `reasoning_shape`'s reason.
    pub context_budget_cap: u32,
    /// Which reasoning field(s) this provider accepts (REQ-559 BR-4). `None`
    /// means not declared; the per-kind default applies at resolution time.
    ///
    /// The adapter reads the *resolved* `ResolvedEffort` off the request rather
    /// than consulting this field — it is carried here only so the projection
    /// back to [`ProviderCapabilities`] stays lossless.
    pub reasoning_shape: Option<ReasoningShape>,
    /// The canonical levels this provider accepts (REQ-559 BR-5). `None` means
    /// not declared.
    ///
    /// A bitset, so this struct keeps its `Copy` derive (REQ-559 ADR-C).
    pub effort_ladder: Option<EffortLadder>,
}

impl CapabilityProfile {
    /// Build a profile from `teton-core`'s [`ProviderCapabilities`].
    #[must_use]
    pub fn from_core(caps: ProviderCapabilities) -> Self {
        Self {
            tool_call_tier: caps.tool_call_tier,
            parallel_calls: caps.parallel_calls,
            max_context: caps.max_context,
            context_budget_cap: caps.context_budget_cap,
            reasoning_shape: caps.reasoning_shape,
            effort_ladder: caps.effort_ladder,
        }
    }

    /// Project back to `teton-core`'s [`ProviderCapabilities`].
    #[must_use]
    pub fn to_core(self) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_call_tier: self.tool_call_tier,
            parallel_calls: self.parallel_calls,
            max_context: self.max_context,
            context_budget_cap: self.context_budget_cap,
            reasoning_shape: self.reasoning_shape,
            effort_ladder: self.effort_ladder,
        }
    }

    /// Derive the harness profile this provider should run under (BR-6).
    #[must_use]
    pub fn harness_profile(self) -> HarnessProfile {
        match self.tool_call_tier {
            ToolCallTier::Native => HarnessProfile {
                max_tools: None,
                max_tool_iterations: NATIVE_MAX_ITERATIONS,
                require_verification: false,
                allow_parallel_tool_calls: self.parallel_calls,
            },
            ToolCallTier::Degraded => HarnessProfile {
                max_tools: Some(DEGRADED_MAX_TOOLS),
                max_tool_iterations: DEGRADED_MAX_ITERATIONS,
                require_verification: true,
                allow_parallel_tool_calls: false,
            },
            ToolCallTier::None => HarnessProfile {
                max_tools: Some(0),
                max_tool_iterations: 0,
                require_verification: true,
                allow_parallel_tool_calls: false,
            },
        }
    }
}

/// The concrete harness constraints derived from a [`CapabilityProfile`]. Weaker
/// tool-callers get a smaller, shorter, verified loop (BR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessProfile {
    /// Cap on tools exposed to the model; `None` means unrestricted.
    pub max_tools: Option<u32>,
    /// Maximum tool-call loop iterations before forcing completion.
    pub max_tool_iterations: u32,
    /// Whether an explicit verification step is mandatory (BR-6).
    pub require_verification: bool,
    /// Whether parallel tool calls are permitted this turn.
    pub allow_parallel_tool_calls: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_roundtrip_is_lossless() {
        // REQ-559: the effort declaration must survive the round trip too. A
        // lossy projection here would silently drop a user's declared ladder
        // between the config and the adapter, and the clamp would then quietly
        // fall back to the per-kind default — a downgrade with nothing observing
        // it (LESSON-456).
        //
        // REQ-586: the user's budget cap must survive it too, for the same
        // reason — a cap dropped here would let a route run over a ceiling the
        // user declared, with nothing observing it. Non-zero on purpose, so a
        // projection that reset it to the default would fail rather than
        // coincide.
        let caps = ProviderCapabilities {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: true,
            max_context: 128_000,
            context_budget_cap: 64_000,
            reasoning_shape: Some(ReasoningShape::ThinkingFlagOnly),
            effort_ladder: Some(EffortLadder::from_levels(&[
                teton_core::EffortLevel::Low,
                teton_core::EffortLevel::Xhigh,
            ])),
        };
        assert_eq!(CapabilityProfile::from_core(caps).to_core(), caps);
        assert_eq!(
            CapabilityProfile::from_core(caps).context_budget_cap,
            64_000
        );

        // And the undeclared case round-trips as undeclared — `None` must not
        // become a materialized default anywhere in the projection.
        let bare = ProviderCapabilities::default();
        let back = CapabilityProfile::from_core(bare).to_core();
        assert_eq!(back, bare);
        assert!(back.reasoning_shape.is_none() && back.effort_ladder.is_none());
        assert_eq!(back.context_budget_cap, 0, "no cap round-trips as no cap");
    }

    #[test]
    fn native_gets_the_full_loop() {
        let p = CapabilityProfile {
            tool_call_tier: ToolCallTier::Native,
            parallel_calls: true,
            max_context: 200_000,
            ..CapabilityProfile::default()
        };
        let h = p.harness_profile();
        assert_eq!(h.max_tools, None);
        assert!(h.max_tool_iterations >= NATIVE_MAX_ITERATIONS);
        assert!(!h.require_verification);
        assert!(h.allow_parallel_tool_calls);
    }

    #[test]
    fn degraded_gets_a_reduced_verified_loop() {
        let p = CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: true, // ignored under degradation
            max_context: 32_000,
            ..CapabilityProfile::default()
        };
        let h = p.harness_profile();
        assert_eq!(h.max_tools, Some(DEGRADED_MAX_TOOLS));
        assert!(h.max_tool_iterations < NATIVE_MAX_ITERATIONS);
        assert!(h.require_verification);
        assert!(
            !h.allow_parallel_tool_calls,
            "degraded providers never run parallel calls"
        );
    }

    #[test]
    fn none_tier_exposes_no_tools() {
        let p = CapabilityProfile {
            tool_call_tier: ToolCallTier::None,
            parallel_calls: false,
            max_context: 8_000,
            ..CapabilityProfile::default()
        };
        let h = p.harness_profile();
        assert_eq!(h.max_tools, Some(0));
        assert_eq!(h.max_tool_iterations, 0);
        assert!(h.require_verification);
    }
}
