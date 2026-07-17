//! Capability profiles and the BR-6 harness-degradation mapping.
//!
//! A provider advertises how well it tool-calls, whether it supports parallel
//! calls, and its context window. From that, [`CapabilityProfile::harness_profile`]
//! derives a [`HarnessProfile`]: providers with weak tool-calling get a reduced
//! harness (smaller tool set, shorter loops, mandatory verification) instead of
//! the full agent loop (BR-6). The [`ToolCallTier`] enum itself is owned by
//! `teton-core`; this crate reuses it rather than duplicating the vocabulary.

use teton_core::{ProviderCapabilities, ToolCallTier};

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
}

impl CapabilityProfile {
    /// Build a profile from `teton-core`'s [`ProviderCapabilities`].
    #[must_use]
    pub fn from_core(caps: ProviderCapabilities) -> Self {
        Self {
            tool_call_tier: caps.tool_call_tier,
            parallel_calls: caps.parallel_calls,
            max_context: caps.max_context,
        }
    }

    /// Project back to `teton-core`'s [`ProviderCapabilities`].
    #[must_use]
    pub fn to_core(self) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_call_tier: self.tool_call_tier,
            parallel_calls: self.parallel_calls,
            max_context: self.max_context,
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
        let caps = ProviderCapabilities {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: true,
            max_context: 128_000,
        };
        assert_eq!(CapabilityProfile::from_core(caps).to_core(), caps);
    }

    #[test]
    fn native_gets_the_full_loop() {
        let p = CapabilityProfile {
            tool_call_tier: ToolCallTier::Native,
            parallel_calls: true,
            max_context: 200_000,
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
        };
        let h = p.harness_profile();
        assert_eq!(h.max_tools, Some(0));
        assert_eq!(h.max_tool_iterations, 0);
        assert!(h.require_verification);
    }
}
