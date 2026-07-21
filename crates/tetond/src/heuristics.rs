//! Freeform-mode routing heuristics (BR-5, BR-8).
//!
//! In *structured* mode routing is decided by the phase → provider policy table
//! (pure logic in `teton_core::policy`, consumed by [`crate::router`]) — never by
//! prompt text. **Freeform** mode is the default experience (BR-3), and here
//! per-prompt heuristics *are* permitted (BR-5). The rule REQ-544 fixes is the
//! division of labor: the always-on local tier handles the cheap "auxiliary"
//! duties (intent classification, file/diff summarization, grep triage, commit
//! messages) where its value is *latency, not intelligence* (BR-8), while an
//! actual coding turn — read/edit/verify — goes to the configured default
//! provider.
//!
//! Two properties are load-bearing and are what this module guarantees:
//!
//! 1. **Every decision is legible** (BR-5): [`route_freeform`] always returns a
//!    provider *and* a user-facing `reason` sentence that names the signal that
//!    fired. The router turns that reason into the `route_decided` event, exactly
//!    as the policy path does. There is no silent heuristic.
//! 2. **The local tier never blocks the loop** (BR-8): when the local tier is
//!    unavailable (below the hardware floor, benchmark-disabled, or shed under
//!    memory pressure), an auxiliary duty that *would* have gone local instead
//!    **bypasses** it to the configured default rather than stalling. Local value
//!    is latency; with no latency win available, the remote provider serves.
//!
//! The module is pure — no I/O, no clock, no globals — so the heuristics are
//! exhaustively unit-testable and the reason strings (part of the contract) are
//! asserted directly.

/// What kind of work a freeform prompt is asking for. Drives the local-vs-remote
/// split (BR-8): auxiliary duties are the local tier's job, coding turns are the
/// configured default's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeformDuty {
    /// A cheap, latency-sensitive local duty: classify, summarize, triage a grep,
    /// draft a commit message. The local tier's reason for existing (BR-8).
    Auxiliary,
    /// A real coding turn — read/edit/verify a file. Routed to the configured
    /// default provider.
    Coding,
}

impl FreeformDuty {
    /// The lowercase label used in reason sentences.
    #[must_use]
    fn label(self) -> &'static str {
        match self {
            FreeformDuty::Auxiliary => "auxiliary",
            FreeformDuty::Coding => "coding",
        }
    }
}

/// The signals that mark a prompt as an [`FreeformDuty::Auxiliary`] duty. Matched
/// case-insensitively as substrings; the first hit is named in the reason so the
/// decision is legible (BR-5).
const AUXILIARY_SIGNALS: &[&str] = &[
    "summarize",
    "summary",
    "classify",
    "classification",
    "triage",
    "commit message",
    "what does",
    "explain",
    "describe",
    "grep",
];

/// The provider identities the freeform router chooses between, plus whether the
/// local tier can currently serve its latency duty (BR-8).
#[derive(Debug, Clone)]
pub struct FreeformConfig {
    /// Id of the local tier provider (serves auxiliary duties when available).
    pub local_provider_id: String,
    /// Id of the configured default provider (serves coding turns, and auxiliary
    /// duties when the local tier is bypassed).
    pub default_provider_id: String,
    /// Whether the local tier can meet its BR-8 latency duty right now. `false`
    /// when it is below the hardware floor, benchmark-disabled, or shed under
    /// memory pressure.
    pub local_available: bool,
}

/// One freeform routing decision: the provider to use plus a legible reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeformDecision {
    /// The classified duty that drove the decision.
    pub duty: FreeformDuty,
    /// Provider id selected. Always populated — freeform routing never returns
    /// "no provider" (BR-8: it bypasses the local tier rather than blocking).
    pub provider_id: String,
    /// Whether an auxiliary duty was routed off the local tier because the local
    /// tier was unavailable (the BR-8 bypass).
    pub bypassed_local: bool,
    /// User-facing sentence naming the signal that fired (feeds `route_decided`).
    pub reason: String,
}

/// Classify a freeform prompt into a [`FreeformDuty`].
///
/// Returns [`FreeformDuty::Auxiliary`] if any [`AUXILIARY_SIGNALS`] keyword
/// appears (case-insensitive), otherwise [`FreeformDuty::Coding`]. Also returns
/// the matched signal (when auxiliary) so callers can name it in the reason.
#[must_use]
pub fn classify_duty(prompt: &str) -> FreeformDuty {
    matched_auxiliary_signal(prompt).map_or(FreeformDuty::Coding, |_| FreeformDuty::Auxiliary)
}

/// The first auxiliary signal present in `prompt` (case-insensitive), if any.
fn matched_auxiliary_signal(prompt: &str) -> Option<&'static str> {
    let lower = prompt.to_lowercase();
    AUXILIARY_SIGNALS
        .iter()
        .copied()
        .find(|signal| lower.contains(signal))
}

/// Route one freeform prompt, always returning a provider and a legible reason.
///
/// - An **auxiliary** duty routes to the local tier for low latency (BR-8) —
///   unless the local tier is unavailable, in which case it **bypasses** to the
///   configured default rather than blocking the loop (still BR-8: local value is
///   latency, and with none available the remote provider serves).
/// - A **coding** turn always routes to the configured default.
///
/// Every branch produces a `reason` naming the signal that fired (BR-5).
#[must_use]
pub fn route_freeform(prompt: &str, config: &FreeformConfig) -> FreeformDecision {
    let signal = matched_auxiliary_signal(prompt);
    let duty = signal.map_or(FreeformDuty::Coding, |_| FreeformDuty::Auxiliary);

    match duty {
        FreeformDuty::Auxiliary if config.local_available => {
            let signal = signal.unwrap_or("an auxiliary duty");
            FreeformDecision {
                duty,
                provider_id: config.local_provider_id.clone(),
                bypassed_local: false,
                reason: format!(
                    "Freeform routing: this looks like an {} duty (matched '{signal}'), so it \
                     goes to the local tier '{}' for low latency (BR-8).",
                    duty.label(),
                    config.local_provider_id,
                ),
            }
        }
        FreeformDuty::Auxiliary => {
            // Local tier unavailable — bypass it rather than block the loop (BR-8).
            let signal = signal.unwrap_or("an auxiliary duty");
            FreeformDecision {
                duty,
                provider_id: config.default_provider_id.clone(),
                bypassed_local: true,
                reason: format!(
                    "Freeform routing: this {} duty (matched '{signal}') would use the local \
                     tier, but it is unavailable, so the router bypasses it to '{}' rather than \
                     blocking the loop (BR-8).",
                    duty.label(),
                    config.default_provider_id,
                ),
            }
        }
        FreeformDuty::Coding => FreeformDecision {
            duty,
            provider_id: config.default_provider_id.clone(),
            bypassed_local: false,
            reason: format!(
                "Freeform routing: this is a {} turn, so it goes to the configured default \
                 provider '{}'.",
                duty.label(),
                config.default_provider_id,
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(local_available: bool) -> FreeformConfig {
        FreeformConfig {
            local_provider_id: "local".to_owned(),
            default_provider_id: "deepseek".to_owned(),
            local_available,
        }
    }

    #[test]
    fn auxiliary_prompts_are_classified_by_signal() {
        for prompt in [
            "Summarize this diff for me",
            "please CLASSIFY the intent of this message",
            "write a commit message for these changes",
            "explain what this function does",
            "triage the grep results",
        ] {
            assert_eq!(
                classify_duty(prompt),
                FreeformDuty::Auxiliary,
                "prompt: {prompt}"
            );
        }
    }

    #[test]
    fn coding_prompts_fall_through_to_coding() {
        for prompt in [
            "add a retry to the http client",
            "fix the off-by-one in parse_header",
            "implement the new endpoint",
        ] {
            assert_eq!(
                classify_duty(prompt),
                FreeformDuty::Coding,
                "prompt: {prompt}"
            );
        }
    }

    #[test]
    fn auxiliary_duty_goes_local_when_available_with_a_legible_reason() {
        let d = route_freeform("summarize the build log", &config(true));
        assert_eq!(d.duty, FreeformDuty::Auxiliary);
        assert_eq!(d.provider_id, "local");
        assert!(!d.bypassed_local);
        // Legible: names the tier and the matched signal, ends as a sentence (BR-5).
        assert!(d.reason.contains("local"), "reason: {}", d.reason);
        assert!(d.reason.contains("summarize"), "reason: {}", d.reason);
        assert!(d.reason.ends_with(')') || d.reason.ends_with('.'));
    }

    #[test]
    fn auxiliary_duty_bypasses_local_when_unavailable_never_blocking() {
        // BR-8: the local tier is unavailable, so an auxiliary duty bypasses it to
        // the default. Crucially a provider is STILL returned (no blocking).
        let d = route_freeform("summarize the build log", &config(false));
        assert_eq!(d.duty, FreeformDuty::Auxiliary);
        assert_eq!(d.provider_id, "deepseek", "bypassed to the default");
        assert!(d.bypassed_local);
        assert!(
            d.reason.contains("unavailable") && d.reason.contains("bypass"),
            "reason must explain the BR-8 bypass: {}",
            d.reason
        );
    }

    #[test]
    fn coding_turn_goes_to_the_default_regardless_of_local_availability() {
        for available in [true, false] {
            let d = route_freeform("implement the parser", &config(available));
            assert_eq!(d.duty, FreeformDuty::Coding);
            assert_eq!(d.provider_id, "deepseek");
            assert!(!d.bypassed_local);
            assert!(d.reason.contains("coding"), "reason: {}", d.reason);
        }
    }

    #[test]
    fn every_decision_carries_a_nonempty_reason() {
        // BR-5: no silent heuristic — every branch produces a reason.
        for available in [true, false] {
            for prompt in ["summarize x", "implement y"] {
                let d = route_freeform(prompt, &config(available));
                assert!(!d.reason.is_empty());
                assert!(!d.provider_id.is_empty());
            }
        }
    }
}
