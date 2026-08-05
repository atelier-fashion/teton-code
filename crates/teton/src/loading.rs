//! The local tier's loading indicator: a pure state machine (REQ-556).
//!
//! For roughly forty seconds after every daemon start the local tier is
//! deep-verifying, loading and benchmarking multi-gigabyte weights. The daemon
//! narrates most of that on `model_lifecycle` — but **not all of it**: between
//! [`ModelLifecycleStage::Verifying`] and [`ModelLifecycleStage::Benchmark`] it
//! publishes nothing at all, and that silence is most of the wait. This module
//! is what fills it (REQ-556 ADR-556-2).
//!
//! Two properties are load-bearing and are why this is a module rather than a
//! few lines in the entry loop:
//!
//! 1. **No I/O, no terminal, no clock.** [`LoadingIndicator::frame`] is a pure
//!    function of the stages observed so far and a caller-supplied tick. BR-2
//!    makes the indicator emit nothing when stdout is not a terminal, so every
//!    piped test is structurally blind to it; computing frames inside the
//!    render path would leave REQ-556's core behaviour with **no verification
//!    route at all** — the TTY gate would double as a test blindfold (BR-11).
//! 2. **It cannot invent progress.** `frame` has no wall clock, so there is
//!    nothing to derive an ETA *from*. BR-5's prohibition on countdowns is
//!    structural here rather than a review note: the load window publishes no
//!    intermediate events and load duration is not recorded across runs, so any
//!    remaining-time figure would be fabricated.
//!
//! This module renders **only** the motion for the silent window. Every
//! lifecycle stage already has a one-line rendering in
//! [`crate::firstrun::render_lifecycle`], and it keeps it — a second renderer
//! for a stage that already has one is exactly the drift BR-10 forbids.

use teton_protocol::events::ModelLifecycleStage;

use crate::firstrun::progress_bar;

/// How many ticks the indicator will animate without hearing anything new
/// before it stops and leaves a static line (BR-7).
///
/// The bound exists because "spin until `Ready` arrives" can spin forever. The
/// daemon publishes `ready` *before* the runtime flips `local_available`, and a
/// client that attaches inside that gap is truthfully replayed "still loading"
/// and never receives another event on its own connection (LESSON-450). Waiting
/// on the event alone therefore has a real, reachable non-termination case; the
/// cap turns it into a stale line instead of a hung animation.
///
/// At the entry loop's frame interval this is a couple of minutes — comfortably
/// longer than any real load, short enough that a wedged daemon stops pretending
/// to make progress.
const MAX_QUIET_TICKS: u64 = 1_200;

/// The frames the indeterminate animation cycles through.
///
/// Growing dots rather than a spinner: the wait is tens of seconds, and a
/// four-state cycle reads as "still working" without the jitter of a fast
/// spinner in a terminal that may be redrawing over a typed line.
const DOTS: [&str; 4] = ["", ".", "..", "..."];

/// What the indicator believes the local tier is doing.
///
/// Derived **only** from received events — never from elapsed time. A tier that
/// has published nothing is not "probably loading"; it is unknown, and unknown
/// draws nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Nothing to draw: no work in progress, or work that is finished, settled,
    /// or waiting on the user (BR-6).
    Hidden,
    /// Weights are transferring. Determinate when the daemon supplied a total.
    Downloading {
        /// Bytes fetched so far, as last published.
        downloaded: u64,
        /// Total bytes when the length is known. `None` ⇒ no fraction may be
        /// shown (BR-5).
        total: Option<u64>,
    },
    /// The whole artifact is on disk and its SHA-256 is being checked. A stage
    /// of its own because hashing 18 GiB is minutes of honest work that would
    /// otherwise read as a hang.
    Verifying {
        /// Size being hashed, for the readout.
        total: u64,
    },
    /// The window this module exists for: verified, not yet serving, and the
    /// daemon has nothing further to say until it benchmarks.
    Loading,
}

/// The loading indicator's state. Cheap to clone, holds no handles.
#[derive(Debug, Clone)]
pub struct LoadingIndicator {
    /// The model the tier is working on, as published (never a path — REQ-547 BR-11).
    model_id: String,
    /// What the tier is doing, per the last stage observed.
    phase: Phase,
    /// Ticks since the last observed stage. Reset by [`Self::observe`].
    quiet_ticks: u64,
    /// Set once [`MAX_QUIET_TICKS`] elapses with no new stage (BR-7). The
    /// indicator then stops advancing but still reports what it last saw.
    stalled: bool,
}

impl Default for LoadingIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadingIndicator {
    /// A fresh indicator that draws nothing until it observes a stage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model_id: String::new(),
            phase: Phase::Hidden,
            quiet_ticks: 0,
            stalled: false,
        }
    }

    /// Fold one lifecycle stage into the indicator's state.
    ///
    /// Called for every `model_lifecycle` event the session renders, so the
    /// indicator and [`crate::firstrun::render_lifecycle`] always describe the
    /// same event — one classifier, two presentations, never two classifiers
    /// (LESSON-456).
    ///
    /// Observing anything at all clears the stall: the daemon spoke, so the
    /// bound restarts.
    pub fn observe(&mut self, model_id: &str, stage: &ModelLifecycleStage) {
        if !model_id.is_empty() {
            self.model_id = model_id.to_owned();
        }
        self.quiet_ticks = 0;
        self.stalled = false;
        self.phase = match stage {
            ModelLifecycleStage::Download {
                downloaded_bytes,
                total_bytes,
            } => Phase::Downloading {
                downloaded: *downloaded_bytes,
                total: *total_bytes,
            },
            ModelLifecycleStage::Verifying { total_bytes } => Phase::Verifying {
                total: *total_bytes,
            },
            // Verified and benchmarked-but-not-yet-ready both land here: the
            // daemon has said all it is going to say until the tier opens.
            // `Benchmark` carries a *result*, so it ends the silent window —
            // but the tier is not serving until `Ready`, and the honest state
            // in between is still "loading".
            ModelLifecycleStage::Benchmark { .. } => Phase::Loading,
            // BR-6: nothing is in progress in any of these. A tier awaiting a
            // decision is waiting on the *user*; animating at someone whose
            // answer is the blocker states the opposite of the truth.
            ModelLifecycleStage::Ready
            | ModelLifecycleStage::AwaitingDecision { .. }
            | ModelLifecycleStage::SteppedDown { .. }
            | ModelLifecycleStage::Disabled { .. } => Phase::Hidden,
            // A probe reports hardware, not work. It is the line before any
            // decision exists, so there is nothing to animate yet.
            ModelLifecycleStage::Probed { .. } => Phase::Hidden,
        };
    }

    /// Advance the animation clock by one frame interval.
    ///
    /// Returns `true` when the caller should repaint. The stalled transition
    /// repaints once — the line changes — and then goes quiet, so a wedged
    /// daemon costs one redraw rather than one per tick forever.
    pub fn tick(&mut self) -> bool {
        if self.phase == Phase::Hidden {
            return false;
        }
        if self.stalled {
            return false;
        }
        self.quiet_ticks = self.quiet_ticks.saturating_add(1);
        if self.quiet_ticks >= MAX_QUIET_TICKS {
            self.stalled = true;
        }
        true
    }

    /// The line to draw at `tick`, or `None` when nothing should be drawn.
    ///
    /// Pure: same state and same tick always yield the same string, and there
    /// is no clock, no filesystem and no terminal anywhere in it (BR-11).
    ///
    /// `tick` may only select an animation frame. It is deliberately **not**
    /// convertible to elapsed or remaining time here — BR-5 forbids an ETA, and
    /// the load window supplies nothing to compute one from.
    #[must_use]
    pub fn frame(&self, tick: u64) -> Option<String> {
        let model = if self.model_id.is_empty() {
            "the local model"
        } else {
            &self.model_id
        };
        match &self.phase {
            Phase::Hidden => None,
            // Stalled: say what was last actually observed and stop moving. The
            // wording avoids claiming progress we have no evidence for.
            _ if self.stalled => Some(format!(
                "{model} is still {} — no update from the daemon for a while; \
                 `teton model status` reports what it sees",
                self.stalled_verb()
            )),
            // Determinate ONLY when the daemon supplied a total (BR-5). The bar
            // is `progress_bar`'s, not a second one (BR-10).
            Phase::Downloading { downloaded, total } => Some(format!(
                "downloading {model} {}{}",
                progress_bar(*downloaded, *total),
                Self::dots(tick)
            )),
            Phase::Verifying { total } => Some(format!(
                "verifying {model} ({} bytes){}",
                total,
                Self::dots(tick)
            )),
            Phase::Loading => Some(format!("model starting{}", Self::dots(tick))),
        }
    }

    /// The animation suffix for a tick. The only use of `tick` in this module.
    fn dots(tick: u64) -> &'static str {
        DOTS[(tick % DOTS.len() as u64) as usize]
    }

    /// How the stalled line describes the last observed phase.
    fn stalled_verb(&self) -> &'static str {
        match self.phase {
            Phase::Downloading { .. } => "downloading",
            Phase::Verifying { .. } => "verifying",
            Phase::Loading => "starting",
            Phase::Hidden => "idle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(downloaded: u64, total: Option<u64>) -> ModelLifecycleStage {
        ModelLifecycleStage::Download {
            downloaded_bytes: downloaded,
            total_bytes: total,
        }
    }

    /// BR-6: an indicator implies work is happening. Only the three
    /// work-in-progress stages may draw; a tier that is finished, settled, or
    /// waiting on the *user* must not animate at someone whose answer is the
    /// blocker.
    #[test]
    fn only_work_in_progress_stages_are_visible() {
        let mut ind = LoadingIndicator::new();
        for stage in [
            download(1, Some(2)),
            ModelLifecycleStage::Verifying { total_bytes: 10 },
            ModelLifecycleStage::Benchmark {
                first_token_ms: 300,
                tokens_per_sec: 70.0,
            },
        ] {
            ind.observe("m", &stage);
            assert!(ind.frame(0).is_some(), "{stage:?} should draw");
            assert!(ind.frame(0).is_some(), "{stage:?} should have a frame");
        }
        for stage in [
            ModelLifecycleStage::Ready,
            ModelLifecycleStage::AwaitingDecision {
                reason: "answer the prompt".to_owned(),
            },
            ModelLifecycleStage::Disabled {
                reason: "below the floor".to_owned(),
            },
            ModelLifecycleStage::SteppedDown {
                from_model: "a".to_owned(),
                to_model: "b".to_owned(),
                reason: "duty missed".to_owned(),
            },
            ModelLifecycleStage::Probed {
                ram_bytes: 1,
                above_floor: true,
            },
        ] {
            ind.observe("m", &stage);
            assert!(ind.frame(0).is_none(), "{stage:?} must not draw");
            assert_eq!(ind.frame(0), None, "{stage:?} must have no frame");
        }
    }

    /// AC-3(a) / BR-5: a fraction is shown only where the daemon supplied the
    /// bytes to compute one. With no total, the readout says `?%` — it does not
    /// guess.
    #[test]
    fn a_fraction_appears_only_when_the_daemon_supplied_a_total() {
        let mut ind = LoadingIndicator::new();
        ind.observe("m", &download(500, Some(1000)));
        let determinate = ind.frame(0).expect("visible");
        assert!(determinate.contains("50%"), "{determinate}");

        ind.observe("m", &download(500, None));
        let indeterminate = ind.frame(0).expect("visible");
        assert!(indeterminate.contains("?%"), "{indeterminate}");
        assert!(
            !indeterminate.contains("50%"),
            "an unknown total must not yield a percentage: {indeterminate}"
        );
    }

    /// AC-3(b) / BR-5: `tick` selects an animation frame and nothing else. No
    /// frame, at any tick, in any phase, may read as a time estimate — there is
    /// no data to build one from, so a plausible-looking number would be a lie.
    #[test]
    fn no_frame_ever_renders_an_eta() {
        let mut ind = LoadingIndicator::new();
        for stage in [
            download(1, Some(1_000_000)),
            download(1, None),
            ModelLifecycleStage::Verifying {
                total_bytes: 18_600_000_000,
            },
            ModelLifecycleStage::Benchmark {
                first_token_ms: 300,
                tokens_per_sec: 70.0,
            },
        ] {
            ind.observe("qwen3-coder-30b-a3b", &stage);
            for tick in 0..500 {
                let f = ind.frame(tick).expect("visible");
                let lower = f.to_lowercase();
                for banned in [
                    "eta",
                    "remaining",
                    "left",
                    "seconds",
                    "minutes",
                    " sec",
                    " min",
                    "estimate",
                ] {
                    assert!(
                        !lower.contains(banned),
                        "frame at tick {tick} reads as a time estimate ({banned}): {f}"
                    );
                }
            }
        }
    }

    /// The frame must actually move — this is the property TASK-042's mutation
    /// check breaks by freezing the tick.
    #[test]
    fn the_frame_advances_with_the_tick() {
        let mut ind = LoadingIndicator::new();
        ind.observe("m", &ModelLifecycleStage::Verifying { total_bytes: 10 });
        let frames: Vec<String> = (0..DOTS.len() as u64)
            .map(|t| ind.frame(t).expect("visible"))
            .collect();
        let distinct: std::collections::BTreeSet<&String> = frames.iter().collect();
        assert_eq!(
            distinct.len(),
            DOTS.len(),
            "each tick in the cycle must render differently: {frames:?}"
        );
        // And it cycles rather than growing without bound.
        assert_eq!(ind.frame(0), ind.frame(DOTS.len() as u64));
    }

    /// AC-6 / BR-7: a tier that never reaches `Ready` must not animate forever.
    /// The daemon publishes `ready` before the runtime applies it, and a client
    /// attaching in that gap hears nothing further (LESSON-450) — so this is a
    /// reachable state, not a hypothetical.
    #[test]
    fn an_indicator_that_never_hears_ready_stops_and_says_what_it_saw() {
        let mut ind = LoadingIndicator::new();
        ind.observe(
            "qwen3-coder-30b-a3b",
            &ModelLifecycleStage::Verifying { total_bytes: 10 },
        );

        let mut repaints = 0;
        for _ in 0..MAX_QUIET_TICKS + 50 {
            if ind.tick() {
                repaints += 1;
            }
        }
        assert!(
            repaints <= MAX_QUIET_TICKS as usize,
            "a stalled indicator must stop asking for repaints, got {repaints}"
        );
        assert!(
            !ind.tick(),
            "a stalled indicator asks for no further repaints"
        );

        let stalled = ind.frame(0).expect("still reports");
        assert!(
            stalled.contains("verifying"),
            "the stalled line must name the last stage actually observed: {stalled}"
        );
        assert!(
            stalled.contains("qwen3-coder-30b-a3b"),
            "and the model it was working on: {stalled}"
        );
    }

    /// Hearing anything at all restarts the bound — the daemon spoke, so the
    /// tier is not wedged.
    #[test]
    fn a_new_stage_clears_a_stall() {
        let mut ind = LoadingIndicator::new();
        ind.observe("m", &ModelLifecycleStage::Verifying { total_bytes: 10 });
        for _ in 0..MAX_QUIET_TICKS + 1 {
            ind.tick();
        }
        assert!(!ind.tick(), "stalled");
        ind.observe("m", &download(1, Some(2)));
        assert!(ind.tick(), "a fresh stage restarts the animation");
    }

    /// A hidden indicator costs nothing per tick — the loop must not repaint a
    /// row that has nothing on it.
    #[test]
    fn a_hidden_indicator_never_asks_for_a_repaint() {
        let mut ind = LoadingIndicator::new();
        assert!(!ind.tick(), "fresh indicator draws nothing");
        ind.observe("m", &ModelLifecycleStage::Ready);
        assert!(!ind.tick(), "a ready tier draws nothing");
    }

    /// BR-11's whole point: none of the above constructed a `Surface`, opened a
    /// terminal, or read a clock. This test states that as an executable claim
    /// about `frame` — it is callable from a context with no I/O at all.
    #[test]
    fn frames_are_computable_with_no_terminal_and_no_clock() {
        let mut ind = LoadingIndicator::new();
        ind.observe("m", &download(3, Some(4)));
        let a = ind.frame(7);
        let b = ind.frame(7);
        assert_eq!(a, b, "frame must be a pure function of (state, tick)");
    }
}
