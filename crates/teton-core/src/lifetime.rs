//! The daemon's lifetime decision, as a pure state machine (REQ-565 BR-9).
//!
//! The daemon exits when its last client leaves. That sentence hides four
//! separate decisions — when to *arm* a shutdown, when to *disarm* it, when to
//! *defer* it because work is in flight, and when to *commit* to it — and all
//! four live here, in a type that owns no socket, no clock, no tokio runtime,
//! and no event bus. [`LifetimeState`] is driven entirely by method calls and
//! answers with a [`LifetimeAction`] describing what the caller must do; the
//! caller ([`tetond::lifetime`]) supplies the timers, the events, and the
//! process exit.
//!
//! That split is the whole point of AC-9: the arm/disarm/defer/commit logic is
//! exercisable without launchd, without a TTY, and without a socket, so none of
//! it can exist only behind an integration test that a CI machine skips.
//!
//! # The three rules that are easy to get wrong
//!
//! 1. **Zero clients is not the same as "the last client left."** A daemon
//!    starts with zero clients, and the CLI that spawned it has not connected
//!    yet. Arming on a bare zero count would make the daemon commit to exiting
//!    before its own reason for existing ever arrived. So the machine starts in
//!    [`LifetimePhase::AwaitingFirstClient`], which never arms; only a
//!    *decrement to zero* does. The startup case gets its own bounded escape
//!    hatch instead ([`LifetimeState::on_startup_grace_elapsed`]), because a
//!    client that never arrives must not strand the daemon forever either.
//!
//! 2. **Blocking work is counted, not flagged.** Two prompt turns can overlap
//!    on one connection, and the daemon must outlive the *last* of them. A bool
//!    would let the first turn to finish clear a claim the second still needs.
//!
//! 3. **Admission and commit are one decision.** [`LifetimeState::admit`] is
//!    the only way in, and it refuses once the state is
//!    [`LifetimePhase::Committed`]. Because the caller holds one mutex across
//!    both, there is no interleaving in which a daemon accepts a session it has
//!    already decided not to serve (BR-3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the daemon does when its last client disconnects (BR-7).
///
/// One knob, three modes. The default is the shipped behaviour; `Linger` is the
/// documented path for scripting users who pay the model load repeatedly; and
/// `Never` is what the `brew services` always-on opt-in passes explicitly, so
/// launchd's keep-alive and the daemon's self-exit cannot fight (BR-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPolicy {
    /// Exit as soon as the last client disconnects and nothing is in flight.
    /// The shipped default; the idle grace is 0 s by product decision (OQ-1).
    OnLastDisconnect,
    /// Exit `seconds` after the last client disconnects, unless one returns.
    Linger {
        /// The idle window, in seconds.
        seconds: u64,
    },
    /// Never self-terminate. The daemon lives until it is signalled.
    Never,
}

impl ShutdownPolicy {
    /// Whether this policy ever exits on its own.
    ///
    /// The one question every transition asks, kept as a named method so the
    /// `Never` short-circuit reads the same at all five call sites.
    #[must_use]
    pub const fn self_terminates(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// Where the effective [`ShutdownPolicy`] came from, for diagnostics.
///
/// A lifetime that surprises an operator is nearly always a policy resolved
/// from somewhere they did not look, so the daemon reports the source alongside
/// the mode rather than the mode alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// Nothing said otherwise.
    Default,
    /// The `[lifetime]` table in the config file.
    Config,
    /// `TETON_SHUTDOWN_POLICY` / `TETON_LINGER_SECONDS`.
    Env,
    /// A `--shutdown-policy` / `--linger-seconds` command-line flag.
    ///
    /// Not in REQ-565's System Model, which lists `default | config | env`.
    /// Added because OQ-2's resolution requires the Homebrew service block to
    /// pass the policy explicitly, and a flag is the better of the two spellings
    /// it allows: it is visible in the launchd plist and in `ps`, it cannot leak
    /// into unrelated child processes the way an exported variable can, and it
    /// cannot be silently dropped by a launchd environment that does not
    /// propagate it.
    Flag,
}

impl PolicySource {
    /// The spelling used in the daemon's startup line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Config => "config",
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

/// Work that must finish before the daemon may exit (BR-2).
///
/// Ordered so that [`LifetimeState::blocking_activity`] reports the same
/// blocker for the same set every time — an event payload that reshuffles
/// between runs is a payload nobody can assert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingActivity {
    /// A prompt turn is executing.
    Turn,
    /// Model weights are downloading or being verified.
    ModelDownload,
    /// Model weights are being loaded or benchmarked.
    ModelLoad,
    /// Cost-ledger writes are outstanding.
    ///
    /// Declared for vocabulary completeness (REQ-565's Events table names it),
    /// but structurally empty as things stand: the ledger is SQLite in
    /// autocommit, so a row is durable the moment `record` returns and there is
    /// no buffer to flush. What actually threatens ledger integrity is a turn
    /// killed before it records — which is why [`Self::Turn`] defers.
    LedgerFlush,
}

impl BlockingActivity {
    /// The wire spelling used in `daemon_shutdown_deferred`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::ModelDownload => "model_download",
            Self::ModelLoad => "model_load",
            Self::LedgerFlush => "ledger_flush",
        }
    }
}

/// Why the daemon exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The last client disconnected (the REQ-565 path).
    LastClient,
    /// No client ever arrived within the startup grace.
    ///
    /// Not in the spec's `reason` enum (`last_client | signal`), and
    /// deliberately distinct from both: a daemon nobody ever talked to did not
    /// lose a last client, and reporting it as `last_client` would make the
    /// commonest orphan — a CLI killed during its autostart poll — look like a
    /// normal session end in the logs.
    StartupUnclaimed,
    /// A signal asked the daemon to stop.
    Signal,
}

impl ExitReason {
    /// The wire spelling used in `daemon_shutdown`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LastClient => "last_client",
            Self::StartupUnclaimed => "startup_unclaimed",
            Self::Signal => "signal",
        }
    }
}

/// Where the daemon is in its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifetimePhase {
    /// Bound and listening, but no client has ever completed a handshake.
    /// Never arms on a zero count — see rule 1 in the module docs.
    AwaitingFirstClient,
    /// At least one client is connected.
    Serving,
    /// The last client left, nothing blocks, and a linger timer is running.
    /// Reachable only under [`ShutdownPolicy::Linger`]; with a 0 s grace the
    /// machine goes straight from `Serving` to `Committed`.
    Armed,
    /// The last client left but work is in flight; exit waits for it.
    Deferred,
    /// The daemon has decided to exit. Admission is closed from here.
    Committed,
}

/// Whether a connecting client was let in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The client is counted and pins the daemon.
    Admitted,
    /// The daemon has committed to exiting; the client must go elsewhere.
    Refused,
}

/// What the caller must do after a transition.
///
/// The state machine cannot emit an event, start a timer, or exit a process —
/// it says what should happen and the supervisor does it. Returning this rather
/// than taking callbacks is what keeps the type testable with no runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifetimeAction {
    /// Nothing to do.
    None,
    /// A shutdown was cancelled; stop any running linger timer.
    Disarm,
    /// Start a linger timer for this many seconds.
    Arm {
        /// How long to wait before re-evaluating.
        linger_seconds: u64,
    },
    /// Shutdown is wanted but blocked; report the blocker.
    Defer {
        /// The activity holding the daemon open.
        blocking_activity: BlockingActivity,
    },
    /// Exit now, for this reason.
    Commit {
        /// Why the daemon is exiting.
        reason: ExitReason,
    },
}

/// The lifetime decision. See the module docs.
///
/// Holds no clock: elapsed time enters through
/// [`Self::on_linger_elapsed`] and [`Self::on_startup_grace_elapsed`], which
/// the supervisor calls when its timers fire.
#[derive(Debug, Clone)]
pub struct LifetimeState {
    policy: ShutdownPolicy,
    source: PolicySource,
    phase: LifetimePhase,
    clients: u32,
    /// Live claims per activity. A `BTreeMap` rather than a set because rule 2
    /// requires counting: the entry is removed only when its count reaches zero.
    blocking: BTreeMap<BlockingActivity, u32>,
}

impl LifetimeState {
    /// A daemon that has just bound its socket and has no clients yet.
    #[must_use]
    pub fn new(policy: ShutdownPolicy, source: PolicySource) -> Self {
        Self {
            policy,
            source,
            phase: LifetimePhase::AwaitingFirstClient,
            clients: 0,
            blocking: BTreeMap::new(),
        }
    }

    /// The effective policy.
    #[must_use]
    pub const fn policy(&self) -> ShutdownPolicy {
        self.policy
    }

    /// Where the policy came from.
    #[must_use]
    pub const fn source(&self) -> PolicySource {
        self.source
    }

    /// The current phase.
    #[must_use]
    pub const fn phase(&self) -> LifetimePhase {
        self.phase
    }

    /// Live client connections — the `live_connection_count` the
    /// `client_connected` / `client_disconnected` events carry.
    #[must_use]
    pub const fn client_count(&self) -> u32 {
        self.clients
    }

    /// The activity that would be reported as blocking exit, if any.
    ///
    /// Deterministic: [`BlockingActivity`]'s ordering picks the same
    /// representative every time for the same set.
    #[must_use]
    pub fn blocking_activity(&self) -> Option<BlockingActivity> {
        self.blocking.keys().copied().next()
    }

    /// Whether anything is in flight.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        !self.blocking.is_empty()
    }

    /// Whether the daemon has committed to exiting.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(self.phase, LifetimePhase::Committed)
    }

    /// Admit a client whose handshake just completed (BR-1, BR-3).
    ///
    /// The count moves at *handshake completion*, never at `accept`: a bare
    /// socket probe that never handshakes — the CLI's own autostart poll and the
    /// e2e harness's readiness check both do exactly this — must not pin the
    /// daemon, and must not arm a shutdown when it drops.
    ///
    /// A client arriving into an [`LifetimePhase::Armed`] or
    /// [`LifetimePhase::Deferred`] daemon **cancels** the pending shutdown.
    /// Once [`LifetimePhase::Committed`], it is refused instead — the two arms
    /// of BR-3, made exhaustive by the caller holding one lock across this call
    /// and the commit.
    pub fn admit(&mut self) -> (Admission, LifetimeAction) {
        if self.is_committed() {
            return (Admission::Refused, LifetimeAction::None);
        }
        let was_pending = matches!(self.phase, LifetimePhase::Armed | LifetimePhase::Deferred);
        self.clients += 1;
        self.phase = LifetimePhase::Serving;
        let action = if was_pending {
            LifetimeAction::Disarm
        } else {
            LifetimeAction::None
        };
        (Admission::Admitted, action)
    }

    /// Record a client disconnecting.
    ///
    /// Only a decrement *to zero* can arm a shutdown; see rule 1.
    pub fn on_disconnect(&mut self) -> LifetimeAction {
        self.clients = self.clients.saturating_sub(1);
        if self.is_committed() || self.clients > 0 {
            return LifetimeAction::None;
        }
        self.evaluate(ExitReason::LastClient)
    }

    /// Claim `activity` as in flight. Idempotent per claim: two claims of the
    /// same activity need two [`Self::end_activity`] calls (rule 2).
    pub fn begin_activity(&mut self, activity: BlockingActivity) {
        *self.blocking.entry(activity).or_insert(0) += 1;
    }

    /// Release one claim on `activity`.
    ///
    /// Releasing the last blocker while a shutdown is pending is what finally
    /// commits it — the `daemon_shutdown_deferred` → exit edge in AC-3.
    pub fn end_activity(&mut self, activity: BlockingActivity) -> LifetimeAction {
        if let Some(count) = self.blocking.get_mut(&activity) {
            *count -= 1;
            if *count == 0 {
                self.blocking.remove(&activity);
            }
        }
        if !matches!(self.phase, LifetimePhase::Deferred) {
            return LifetimeAction::None;
        }
        self.evaluate(ExitReason::LastClient)
    }

    /// The linger timer elapsed.
    ///
    /// Re-evaluates rather than committing blindly: a client may have returned
    /// (which would have moved the phase to `Serving`) or work may have started.
    pub fn on_linger_elapsed(&mut self) -> LifetimeAction {
        if !matches!(self.phase, LifetimePhase::Armed) {
            return LifetimeAction::None;
        }
        self.commit_or_defer(ExitReason::LastClient)
    }

    /// The startup grace elapsed with no client ever having arrived.
    ///
    /// Bounded escape hatch for the orphan case: a CLI that spawns the daemon
    /// and then dies during its own autostart poll would otherwise leave a
    /// daemon holding the model resident forever, which is precisely the harm
    /// REQ-565 exists to remove.
    pub fn on_startup_grace_elapsed(&mut self) -> LifetimeAction {
        if !matches!(self.phase, LifetimePhase::AwaitingFirstClient)
            || !self.policy.self_terminates()
        {
            return LifetimeAction::None;
        }
        self.commit_or_defer(ExitReason::StartupUnclaimed)
    }

    /// A signal asked the daemon to stop. Commits unconditionally — including
    /// under [`ShutdownPolicy::Never`], which governs *self*-termination only.
    pub fn on_signal(&mut self) -> LifetimeAction {
        self.phase = LifetimePhase::Committed;
        LifetimeAction::Commit {
            reason: ExitReason::Signal,
        }
    }

    /// Decide what an idle daemon should do, honouring the policy.
    fn evaluate(&mut self, reason: ExitReason) -> LifetimeAction {
        if !self.policy.self_terminates() {
            return LifetimeAction::None;
        }
        match self.policy {
            // 0 s grace by product decision (OQ-1): no Armed hop.
            ShutdownPolicy::OnLastDisconnect => self.commit_or_defer(reason),
            ShutdownPolicy::Linger { seconds } => {
                if let Some(blocking_activity) = self.blocking_activity() {
                    self.phase = LifetimePhase::Deferred;
                    return LifetimeAction::Defer { blocking_activity };
                }
                self.phase = LifetimePhase::Armed;
                LifetimeAction::Arm {
                    linger_seconds: seconds,
                }
            }
            ShutdownPolicy::Never => LifetimeAction::None,
        }
    }

    /// Commit, unless work is in flight — in which case defer to it (BR-2).
    fn commit_or_defer(&mut self, reason: ExitReason) -> LifetimeAction {
        if let Some(blocking_activity) = self.blocking_activity() {
            self.phase = LifetimePhase::Deferred;
            return LifetimeAction::Defer { blocking_activity };
        }
        self.phase = LifetimePhase::Committed;
        LifetimeAction::Commit { reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_last() -> LifetimeState {
        LifetimeState::new(ShutdownPolicy::OnLastDisconnect, PolicySource::Default)
    }

    fn connect(state: &mut LifetimeState) {
        let (admission, _) = state.admit();
        assert_eq!(admission, Admission::Admitted);
    }

    // -- rule 1: zero clients is not "the last client left" -------------------

    #[test]
    fn a_fresh_daemon_awaits_its_first_client_and_does_not_arm() {
        let state = on_last();
        assert_eq!(state.phase(), LifetimePhase::AwaitingFirstClient);
        assert_eq!(state.client_count(), 0);
        assert!(!state.is_committed());
    }

    /// The regression this whole phase exists to prevent: a daemon that exits
    /// before the CLI which spawned it ever connects.
    #[test]
    fn a_zero_count_alone_never_commits_only_a_decrement_to_zero_does() {
        let mut state = on_last();
        // Nothing has happened; a stray evaluation must not exit.
        assert_eq!(state.on_linger_elapsed(), LifetimeAction::None);
        assert!(!state.is_committed());

        connect(&mut state);
        assert_eq!(state.phase(), LifetimePhase::Serving);
        assert_eq!(
            state.on_disconnect(),
            LifetimeAction::Commit {
                reason: ExitReason::LastClient
            }
        );
        assert!(state.is_committed());
    }

    #[test]
    fn an_unclaimed_daemon_exits_when_the_startup_grace_elapses() {
        let mut state = on_last();
        assert_eq!(
            state.on_startup_grace_elapsed(),
            LifetimeAction::Commit {
                reason: ExitReason::StartupUnclaimed
            }
        );
    }

    /// Once a client has been served, the startup grace is spent — a later
    /// idle daemon exits as `last_client`, not as an orphan.
    #[test]
    fn the_startup_grace_does_not_fire_after_a_client_has_arrived() {
        let mut state = on_last();
        connect(&mut state);
        assert_eq!(state.on_startup_grace_elapsed(), LifetimeAction::None);
        assert!(!state.is_committed());
    }

    // -- rule 2: blocking work is counted, not flagged ------------------------

    #[test]
    fn two_overlapping_turns_need_two_releases_before_exit() {
        let mut state = on_last();
        connect(&mut state);
        state.begin_activity(BlockingActivity::Turn);
        state.begin_activity(BlockingActivity::Turn);

        assert_eq!(
            state.on_disconnect(),
            LifetimeAction::Defer {
                blocking_activity: BlockingActivity::Turn
            }
        );
        // First release: the second turn still holds the daemon open.
        assert_eq!(
            state.end_activity(BlockingActivity::Turn),
            LifetimeAction::Defer {
                blocking_activity: BlockingActivity::Turn
            }
        );
        assert!(!state.is_committed());
        // Second release: now it may go.
        assert_eq!(
            state.end_activity(BlockingActivity::Turn),
            LifetimeAction::Commit {
                reason: ExitReason::LastClient
            }
        );
    }

    /// AC-3's shape: the last client leaves mid-turn, the daemon defers, the
    /// turn finishes, the daemon exits.
    #[test]
    fn an_in_flight_turn_defers_exit_until_it_completes() {
        let mut state = on_last();
        connect(&mut state);
        state.begin_activity(BlockingActivity::Turn);

        let action = state.on_disconnect();
        assert_eq!(
            action,
            LifetimeAction::Defer {
                blocking_activity: BlockingActivity::Turn
            }
        );
        assert_eq!(state.phase(), LifetimePhase::Deferred);

        assert_eq!(
            state.end_activity(BlockingActivity::Turn),
            LifetimeAction::Commit {
                reason: ExitReason::LastClient
            }
        );
    }

    #[test]
    fn a_download_defers_exit_just_as_a_turn_does() {
        for activity in [
            BlockingActivity::ModelDownload,
            BlockingActivity::ModelLoad,
            BlockingActivity::LedgerFlush,
        ] {
            let mut state = on_last();
            connect(&mut state);
            state.begin_activity(activity);
            assert_eq!(
                state.on_disconnect(),
                LifetimeAction::Defer {
                    blocking_activity: activity
                },
                "{activity:?} must defer exit"
            );
            assert_eq!(
                state.end_activity(activity),
                LifetimeAction::Commit {
                    reason: ExitReason::LastClient
                }
            );
        }
    }

    /// Releasing an activity while clients are still connected is not an exit
    /// trigger — only an idle daemon evaluates.
    #[test]
    fn ending_work_while_a_client_remains_does_nothing() {
        let mut state = on_last();
        connect(&mut state);
        state.begin_activity(BlockingActivity::Turn);
        assert_eq!(
            state.end_activity(BlockingActivity::Turn),
            LifetimeAction::None
        );
        assert_eq!(state.phase(), LifetimePhase::Serving);
    }

    #[test]
    fn the_reported_blocker_is_deterministic_for_a_given_set() {
        let mut state = on_last();
        state.begin_activity(BlockingActivity::ModelLoad);
        state.begin_activity(BlockingActivity::Turn);
        // Declaration order, not insertion order.
        assert_eq!(state.blocking_activity(), Some(BlockingActivity::Turn));
    }

    // -- rule 3: admission and commit are one decision ------------------------

    #[test]
    fn a_committed_daemon_refuses_admission() {
        let mut state = on_last();
        connect(&mut state);
        state.on_disconnect();
        assert!(state.is_committed());

        let (admission, action) = state.admit();
        assert_eq!(admission, Admission::Refused);
        assert_eq!(action, LifetimeAction::None);
        assert_eq!(state.client_count(), 0, "a refused client is never counted");
    }

    /// BR-3's first arm: a client arriving before the commit cancels it.
    #[test]
    fn a_client_arriving_while_deferred_cancels_the_shutdown() {
        let mut state = on_last();
        connect(&mut state);
        state.begin_activity(BlockingActivity::Turn);
        assert_eq!(state.phase(), LifetimePhase::Serving);
        state.on_disconnect();
        assert_eq!(state.phase(), LifetimePhase::Deferred);

        let (admission, action) = state.admit();
        assert_eq!(admission, Admission::Admitted);
        assert_eq!(action, LifetimeAction::Disarm);
        assert_eq!(state.phase(), LifetimePhase::Serving);

        // And the now-finished turn must not resurrect the shutdown.
        assert_eq!(
            state.end_activity(BlockingActivity::Turn),
            LifetimeAction::None
        );
        assert!(!state.is_committed());
    }

    // -- multi-client (AC-2) --------------------------------------------------

    #[test]
    fn only_the_last_disconnect_stops_a_multi_client_daemon() {
        let mut state = on_last();
        connect(&mut state);
        connect(&mut state);
        assert_eq!(state.client_count(), 2);

        assert_eq!(state.on_disconnect(), LifetimeAction::None);
        assert_eq!(state.client_count(), 1);
        assert!(!state.is_committed());

        assert_eq!(
            state.on_disconnect(),
            LifetimeAction::Commit {
                reason: ExitReason::LastClient
            }
        );
    }

    // -- policies (AC-8) ------------------------------------------------------

    #[test]
    fn never_survives_the_last_disconnect_and_the_startup_grace() {
        let mut state = LifetimeState::new(ShutdownPolicy::Never, PolicySource::Flag);
        assert_eq!(state.on_startup_grace_elapsed(), LifetimeAction::None);
        connect(&mut state);
        assert_eq!(state.on_disconnect(), LifetimeAction::None);
        assert!(!state.is_committed());
        assert_eq!(state.source(), PolicySource::Flag);
    }

    /// `Never` governs *self*-termination; a signal still stops the daemon,
    /// which is what makes `brew services stop` work under the always-on opt-in.
    #[test]
    fn never_still_honours_a_signal() {
        let mut state = LifetimeState::new(ShutdownPolicy::Never, PolicySource::Flag);
        connect(&mut state);
        assert_eq!(
            state.on_signal(),
            LifetimeAction::Commit {
                reason: ExitReason::Signal
            }
        );
        assert!(state.is_committed());
    }

    #[test]
    fn linger_arms_a_timer_and_exits_when_it_elapses() {
        let mut state =
            LifetimeState::new(ShutdownPolicy::Linger { seconds: 30 }, PolicySource::Config);
        connect(&mut state);
        assert_eq!(
            state.on_disconnect(),
            LifetimeAction::Arm { linger_seconds: 30 }
        );
        assert_eq!(state.phase(), LifetimePhase::Armed);
        assert_eq!(
            state.on_linger_elapsed(),
            LifetimeAction::Commit {
                reason: ExitReason::LastClient
            }
        );
    }

    #[test]
    fn a_client_returning_inside_the_linger_window_keeps_the_daemon() {
        let mut state =
            LifetimeState::new(ShutdownPolicy::Linger { seconds: 30 }, PolicySource::Env);
        connect(&mut state);
        state.on_disconnect();
        assert_eq!(state.phase(), LifetimePhase::Armed);

        let (admission, action) = state.admit();
        assert_eq!(admission, Admission::Admitted);
        assert_eq!(action, LifetimeAction::Disarm);

        // A late timer for the cancelled arm is a no-op, not an exit.
        assert_eq!(state.on_linger_elapsed(), LifetimeAction::None);
        assert!(!state.is_committed());
    }

    #[test]
    fn linger_defers_to_in_flight_work_rather_than_arming() {
        let mut state =
            LifetimeState::new(ShutdownPolicy::Linger { seconds: 5 }, PolicySource::Config);
        connect(&mut state);
        state.begin_activity(BlockingActivity::ModelDownload);
        assert_eq!(
            state.on_disconnect(),
            LifetimeAction::Defer {
                blocking_activity: BlockingActivity::ModelDownload
            }
        );
    }

    // -- vocabulary -----------------------------------------------------------

    #[test]
    fn wire_spellings_match_the_specs_event_vocabulary() {
        assert_eq!(BlockingActivity::Turn.as_str(), "turn");
        assert_eq!(BlockingActivity::ModelDownload.as_str(), "model_download");
        assert_eq!(BlockingActivity::ModelLoad.as_str(), "model_load");
        assert_eq!(BlockingActivity::LedgerFlush.as_str(), "ledger_flush");
        assert_eq!(ExitReason::LastClient.as_str(), "last_client");
        assert_eq!(ExitReason::Signal.as_str(), "signal");
        assert_eq!(ExitReason::StartupUnclaimed.as_str(), "startup_unclaimed");
        assert!(ShutdownPolicy::OnLastDisconnect.self_terminates());
        assert!(ShutdownPolicy::Linger { seconds: 1 }.self_terminates());
        assert!(!ShutdownPolicy::Never.self_terminates());
    }

    /// A disconnect that underflows the count must saturate rather than wrap —
    /// a wrapped `u32` would read as ~4 billion clients and pin the daemon
    /// forever.
    #[test]
    fn a_spurious_disconnect_cannot_underflow_the_count() {
        let mut state = on_last();
        state.on_disconnect();
        assert_eq!(state.client_count(), 0);
    }
}
