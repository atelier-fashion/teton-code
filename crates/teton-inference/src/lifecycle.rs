//! Local-model lifecycle progress events (BR-9).
//!
//! The probe, download, benchmark, and memory-pressure paths all report their
//! progress by emitting [`LifecycleEvent`]s through an `on_event` callback. This
//! keeps `teton-inference` a self-contained library: it has no opinion on *how*
//! the events reach a client.
//!
//! These variants map one-to-one onto the daemon-facing `model_lifecycle`
//! protocol event (`teton_protocol::events::ModelLifecycleStage`): `Probed`,
//! `Download`, `Benchmark`, `Ready`, `SteppedDown`, `Disabled`. The daemon
//! translates a [`LifecycleEvent`] into a wire `model_lifecycle` event; keeping
//! the two types decoupled means this crate does not depend on the protocol
//! crate and stays trivially unit-testable.

/// A single step in the local-model lifecycle, reported as it happens.
///
/// `tokens_per_sec` is an `f32`, so this type is [`PartialEq`] but not [`Eq`].
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleEvent {
    /// First-run hardware probe result: RAM/disk/GPU class resolved to a
    /// candidate model (or to the disabled tier below the floor).
    Probed {
        /// The candidate model id, or a sentinel when the tier is disabled.
        model_id: String,
        /// Detected total system RAM in bytes.
        ram_bytes: u64,
        /// Whether the machine cleared the local-tier hardware floor.
        above_floor: bool,
    },
    /// Download progress for the selected model.
    Download {
        /// The model being fetched.
        model_id: String,
        /// Bytes durably written so far (survives resume across interruptions).
        downloaded_bytes: u64,
        /// Total expected bytes, when known.
        total_bytes: Option<u64>,
    },
    /// A post-download or runtime micro-benchmark result (the BR-8 latency duty).
    Benchmark {
        /// The model that was benchmarked.
        model_id: String,
        /// Measured time to first token, in milliseconds.
        first_token_ms: u32,
        /// Measured decode throughput in tokens per second.
        tokens_per_sec: f32,
    },
    /// The model is loaded and serving requests.
    Ready {
        /// The model now serving.
        model_id: String,
    },
    /// The tier auto-stepped down to a smaller model after a failed duty
    /// (benchmark too slow, or memory pressure).
    SteppedDown {
        /// Model stepped away from.
        from_model: String,
        /// Model stepped down to.
        to_model: String,
        /// User-facing reason.
        reason: String,
    },
    /// The local tier is cleanly absent (below the floor, exhausted step-down,
    /// or unloaded under memory pressure). Sessions proceed remote-only.
    Disabled {
        /// User-facing reason.
        reason: String,
    },
}

impl LifecycleEvent {
    /// The snake_case stage name, identical to the `model_lifecycle` protocol
    /// stage tag. Handy for structured logging and for asserting in tests.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            LifecycleEvent::Probed { .. } => "probed",
            LifecycleEvent::Download { .. } => "download",
            LifecycleEvent::Benchmark { .. } => "benchmark",
            LifecycleEvent::Ready { .. } => "ready",
            LifecycleEvent::SteppedDown { .. } => "stepped_down",
            LifecycleEvent::Disabled { .. } => "disabled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_the_protocol_stage_tags() {
        let cases = [
            (
                LifecycleEvent::Probed {
                    model_id: "m".to_owned(),
                    ram_bytes: 1,
                    above_floor: true,
                },
                "probed",
            ),
            (
                LifecycleEvent::Download {
                    model_id: "m".to_owned(),
                    downloaded_bytes: 1,
                    total_bytes: Some(2),
                },
                "download",
            ),
            (
                LifecycleEvent::Benchmark {
                    model_id: "m".to_owned(),
                    first_token_ms: 1,
                    tokens_per_sec: 2.0,
                },
                "benchmark",
            ),
            (
                LifecycleEvent::Ready {
                    model_id: "m".to_owned(),
                },
                "ready",
            ),
            (
                LifecycleEvent::SteppedDown {
                    from_model: "a".to_owned(),
                    to_model: "b".to_owned(),
                    reason: "r".to_owned(),
                },
                "stepped_down",
            ),
            (
                LifecycleEvent::Disabled {
                    reason: "r".to_owned(),
                },
                "disabled",
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(event.kind(), expected);
        }
    }
}
