//! Failure classification (AC-7 backend).
//!
//! A provider failure is classified into a [`FailureClass`] and mapped to a
//! [`FailureDecision`] — retry the same provider, fall back to another, degrade
//! the harness profile (BR-6), or fail. [`degradation_signal`] turns a decision
//! that removes a provider from play (fallback or degrade) into the payload for
//! the `provider_degraded` event, complete with a legible reason sentence — the
//! same "control = legibility" posture as the router's `route_decided`.

use std::fmt;

/// How a provider call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The request timed out.
    Timeout,
    /// A transport-level failure that is not a timeout (connect/reset/I/O).
    Transport,
    /// The provider returned a 4xx status.
    ClientError {
        /// HTTP status code.
        status: u16,
    },
    /// The provider returned a 5xx status.
    ServerError {
        /// HTTP status code.
        status: u16,
    },
    /// The response was not parseable as the expected protocol.
    MalformedResponse,
    /// A tool call's arguments were not valid JSON.
    MalformedToolCall,
}

/// The action to take in response to a [`FailureClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    /// Retry the same provider (transient failure).
    Retry,
    /// Fall back to the configured fallback provider.
    Fallback,
    /// Keep the provider but drop to a reduced harness profile (BR-6).
    Degrade,
    /// Give up — the failure is not recoverable by retry or fallback (e.g. an
    /// auth error, which a different provider would hit too).
    Fail,
}

/// A classified failure plus the action to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureDecision {
    /// The classified failure.
    pub class: FailureClass,
    /// The action the daemon should take.
    pub action: FailureAction,
    /// Whether retrying the same provider could plausibly succeed.
    pub retryable: bool,
}

/// Classify a failure into a decision.
///
/// The mapping encodes AC-7 / BR-6 policy:
/// - timeouts, transport errors, and 5xx are transient → **retry**;
/// - 429/408 are transient rate/timeout signals → **retry**;
/// - 401/403 are auth problems any provider would hit → **fail**;
/// - other 4xx and malformed responses are persistent for this provider →
///   **fallback**;
/// - a malformed tool call means weak tool-calling → **degrade** the harness
///   profile (BR-6) rather than abandon the provider.
#[must_use]
pub fn classify(class: FailureClass) -> FailureDecision {
    let (action, retryable) = match class {
        FailureClass::Timeout | FailureClass::Transport | FailureClass::ServerError { .. } => {
            (FailureAction::Retry, true)
        }
        FailureClass::ClientError { status } => match status {
            408 | 429 => (FailureAction::Retry, true),
            401 | 403 => (FailureAction::Fail, false),
            _ => (FailureAction::Fallback, false),
        },
        FailureClass::MalformedResponse => (FailureAction::Fallback, false),
        FailureClass::MalformedToolCall => (FailureAction::Degrade, false),
    };
    FailureDecision {
        class,
        action,
        retryable,
    }
}

/// Payload for the `provider_degraded` event: a provider that fell back or
/// dropped to a reduced profile, plus a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDegraded {
    /// The provider that degraded.
    pub provider_id: String,
    /// Why it degraded.
    pub class: FailureClass,
    /// What was done about it.
    pub action: FailureAction,
    /// A legible one-sentence reason for the event surface.
    pub reason: String,
}

/// Build a `provider_degraded` signal from a decision, or `None` when the
/// outcome does not remove the provider from play.
///
/// Only [`FailureAction::Fallback`] and [`FailureAction::Degrade`] surface a
/// `provider_degraded` event: a [`FailureAction::Retry`] is transient (nothing
/// to report yet) and a [`FailureAction::Fail`] aborts rather than degrades.
#[must_use]
pub fn degradation_signal(
    provider_id: &str,
    decision: FailureDecision,
) -> Option<ProviderDegraded> {
    let reason = match decision.action {
        FailureAction::Fallback => format!(
            "provider `{provider_id}` fell back after {}",
            describe(decision.class)
        ),
        FailureAction::Degrade => format!(
            "provider `{provider_id}` dropped to a reduced harness profile after {}",
            describe(decision.class)
        ),
        FailureAction::Retry | FailureAction::Fail => return None,
    };
    Some(ProviderDegraded {
        provider_id: provider_id.to_string(),
        class: decision.class,
        action: decision.action,
        reason,
    })
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&describe(*self))
    }
}

/// A short, content-free description of a failure class (safe for logs/events —
/// carries no prompt text or credentials).
fn describe(class: FailureClass) -> String {
    match class {
        FailureClass::Timeout => "a request timeout".to_string(),
        FailureClass::Transport => "a transport error".to_string(),
        FailureClass::ClientError { status } => format!("a {status} client error"),
        FailureClass::ServerError { status } => format!("a {status} server error"),
        FailureClass::MalformedResponse => "a malformed response".to_string(),
        FailureClass::MalformedToolCall => "a malformed tool call".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failures_retry() {
        for class in [
            FailureClass::Timeout,
            FailureClass::Transport,
            FailureClass::ServerError { status: 503 },
            FailureClass::ClientError { status: 429 },
            FailureClass::ClientError { status: 408 },
        ] {
            let d = classify(class);
            assert_eq!(d.action, FailureAction::Retry, "class {class:?}");
            assert!(d.retryable, "class {class:?}");
        }
    }

    #[test]
    fn auth_errors_fail_not_fallback() {
        for status in [401, 403] {
            let d = classify(FailureClass::ClientError { status });
            assert_eq!(d.action, FailureAction::Fail);
            assert!(!d.retryable);
        }
    }

    #[test]
    fn persistent_client_and_malformed_response_fall_back() {
        assert_eq!(
            classify(FailureClass::ClientError { status: 400 }).action,
            FailureAction::Fallback
        );
        assert_eq!(
            classify(FailureClass::MalformedResponse).action,
            FailureAction::Fallback
        );
    }

    #[test]
    fn malformed_tool_call_degrades_the_harness() {
        let d = classify(FailureClass::MalformedToolCall);
        assert_eq!(d.action, FailureAction::Degrade);
        assert!(!d.retryable);
    }

    #[test]
    fn degradation_signal_only_for_fallback_and_degrade() {
        // Fallback -> Some, mentions the provider and reason.
        let sig = degradation_signal("deepseek", classify(FailureClass::MalformedResponse))
            .expect("fallback surfaces provider_degraded");
        assert_eq!(sig.provider_id, "deepseek");
        assert_eq!(sig.action, FailureAction::Fallback);
        assert!(sig.reason.contains("deepseek"));
        assert!(!sig.reason.is_empty());

        // Degrade -> Some.
        let sig = degradation_signal("kimi", classify(FailureClass::MalformedToolCall))
            .expect("degrade surfaces provider_degraded");
        assert_eq!(sig.action, FailureAction::Degrade);

        // Retry and Fail -> None.
        assert!(degradation_signal("anthropic", classify(FailureClass::Timeout)).is_none());
        assert!(degradation_signal(
            "anthropic",
            classify(FailureClass::ClientError { status: 401 })
        )
        .is_none());
    }

    #[test]
    fn descriptions_leak_no_content() {
        // Reason strings must be safe to emit — no prompt/credential material,
        // only the class and status.
        let s = describe(FailureClass::ClientError { status: 404 });
        assert!(s.contains("404"));
    }
}
