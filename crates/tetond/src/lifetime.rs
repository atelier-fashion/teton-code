//! The async half of the daemon's lifetime (REQ-565).
//!
//! [`teton_core::lifetime::LifetimeState`] decides *what* should happen; this
//! module makes it happen — it owns the mutex, hands out the RAII guards that
//! pin the daemon, publishes the [`Event::DaemonLifetime`] stages, runs the
//! linger and startup-grace timers, and signals the accept loop.
//!
//! # Why the guards are RAII
//!
//! Every claim on the daemon's life is released by `Drop`, never by an explicit
//! call. A prompt turn can panic, a future can be cancelled mid-await, a client
//! task can be torn down by an I/O error — and any of those leaking a claim
//! would wedge the daemon alive forever holding the model resident, which is
//! precisely the harm REQ-565 exists to remove. This mirrors
//! [`crate::model_consent`]'s `InFlightGuard`, which exists for the same reason.
//!
//! # Why the mutex is `std::sync::Mutex`
//!
//! The lock is never held across an `.await`. Every critical section is a
//! handful of arithmetic operations on [`LifetimeState`], and the resulting
//! [`LifetimeAction`] is applied *after* the guard drops. A `tokio::sync::Mutex`
//! would buy the ability to hold it across await points — the one thing this
//! module must never do, because admission and commit have to be one
//! uninterruptible decision (BR-3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use teton_core::lifetime::{
    Admission, BlockingActivity, ExitReason, LifetimeAction, LifetimePhase, LifetimeState,
    PolicySource, ShutdownPolicy,
};
use teton_core::{LifetimeConfig, ShutdownPolicyKind};
use teton_protocol::events::{DaemonLifetime, DaemonLifetimeStage, Event};
use tokio::sync::Notify;

use crate::broadcast::EventBus;

/// How long a daemon that has never been contacted waits before giving up.
///
/// An order of magnitude above the CLI's own autostart budget (50 polls ×
/// 100 ms = 5 s, `teton::client::POLL_ATTEMPTS`), so a client that is merely
/// slow always wins the race; the grace only catches a client that will never
/// arrive at all. Measured from the moment the socket is bound, which is what
/// makes that comparison valid — a slow startup happens *before* the bind, with
/// no socket for anyone to connect to.
pub const STARTUP_GRACE: Duration = Duration::from_secs(60);

/// The daemon's `--shutdown-policy` / `--linger-seconds` flags, once parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyFlags {
    /// `--shutdown-policy <mode>`.
    pub shutdown: Option<ShutdownPolicyKind>,
    /// `--linger-seconds <n>`.
    pub linger_seconds: Option<u64>,
}

/// A `--shutdown-policy` value this build does not recognize.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyArgError {
    /// The mode is not one of the three spellings.
    #[error(
        "unknown {origin} value '{value}' — expected one of: {}. \
         Refusing to start rather than silently running under a lifetime you did not ask for.",
        ShutdownPolicyKind::SPELLINGS.join(", ")
    )]
    UnknownMode {
        /// Where the bad value came from, e.g. `--shutdown-policy`.
        origin: String,
        /// The value, echoed so the typo is visible.
        value: String,
    },
    /// A numeric option did not parse.
    #[error("{origin} expects a whole number of seconds, got '{value}'")]
    NotANumber {
        /// Where the bad value came from.
        origin: String,
        /// The value.
        value: String,
    },
    /// A flag that takes a value was given none.
    #[error("{origin} requires a value")]
    MissingValue {
        /// The flag.
        origin: String,
    },
}

/// Parse the lifetime flags out of a command line (REQ-565 BR-7).
///
/// Takes the arguments rather than reading `std::env::args` so the parse is
/// testable without a process — the same reason `seam_policy` in
/// [`crate::runtime`] is pure.
///
/// # Errors
///
/// Returns [`PolicyArgError`] for an unknown mode, a non-numeric window, or a
/// flag with no value. Refusing beats defaulting: a typo'd
/// `--shutdown-policy nevr` that silently fell back to
/// `on-last-disconnect` would give an always-on service the one lifetime it
/// must not have (BR-5).
pub fn parse_policy_flags<I, S>(args: I) -> Result<PolicyFlags, PolicyArgError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut flags = PolicyFlags::default();
    let mut args = args.into_iter().map(|a| a.as_ref().to_owned());
    while let Some(arg) = args.next() {
        // Both `--flag value` and `--flag=value` spellings; launchd plists and
        // hand-typed command lines differ on which they use.
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (arg, None),
        };
        let mut take_value = |origin: &str| -> Result<String, PolicyArgError> {
            inline.clone().map_or_else(
                || {
                    args.next().ok_or_else(|| PolicyArgError::MissingValue {
                        origin: origin.to_owned(),
                    })
                },
                Ok,
            )
        };
        match name.as_str() {
            "--shutdown-policy" => {
                let value = take_value("--shutdown-policy")?;
                flags.shutdown = Some(ShutdownPolicyKind::parse(&value).ok_or(
                    PolicyArgError::UnknownMode {
                        origin: "--shutdown-policy".to_owned(),
                        value,
                    },
                )?);
            }
            "--linger-seconds" => {
                let value = take_value("--linger-seconds")?;
                flags.linger_seconds =
                    Some(value.parse().map_err(|_| PolicyArgError::NotANumber {
                        origin: "--linger-seconds".to_owned(),
                        value,
                    })?);
            }
            _ => {}
        }
    }
    Ok(flags)
}

/// The lifetime settings read from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyEnv {
    /// `TETON_SHUTDOWN_POLICY`.
    pub shutdown: Option<ShutdownPolicyKind>,
    /// `TETON_LINGER_SECONDS`.
    pub linger_seconds: Option<u64>,
}

impl PolicyEnv {
    /// Read the two variables from the process environment.
    ///
    /// These are **operator** settings, not test seams: they are deliberately
    /// not gated behind `TETON_TEST_SEAMS`, because a release build has to
    /// honour them — the shipped Homebrew service block passes the `never`
    /// policy, and a release daemon that ignored it would flap against
    /// launchd's keep-alive (BR-5). Same posture as `TETON_CONFIG`.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyArgError`] for an unrecognized mode or a non-numeric
    /// window, for the same refuse-don't-default reason as the flags.
    pub fn from_env() -> Result<Self, PolicyArgError> {
        let shutdown = match std::env::var("TETON_SHUTDOWN_POLICY") {
            Ok(value) if !value.trim().is_empty() => Some(
                ShutdownPolicyKind::parse(&value).ok_or(PolicyArgError::UnknownMode {
                    origin: "TETON_SHUTDOWN_POLICY".to_owned(),
                    value,
                })?,
            ),
            _ => None,
        };
        let linger_seconds = match std::env::var("TETON_LINGER_SECONDS") {
            Ok(value) if !value.trim().is_empty() => {
                Some(value.parse().map_err(|_| PolicyArgError::NotANumber {
                    origin: "TETON_LINGER_SECONDS".to_owned(),
                    value,
                })?)
            }
            _ => None,
        };
        Ok(Self {
            shutdown,
            linger_seconds,
        })
    }
}

/// Resolve the effective policy from all three sources (REQ-565 BR-7, D-7).
///
/// Precedence is **flag > env > config > default**, most explicit first: a flag
/// is written on the command line that started *this* process, an environment
/// variable belongs to whoever launched it, and the config file is the standing
/// preference. The winning source is returned alongside the policy because a
/// lifetime that surprises an operator is nearly always resolved from somewhere
/// they did not look.
///
/// Pure, so every precedence rule is testable without a process or a file.
#[must_use]
pub fn resolve_policy(
    flags: PolicyFlags,
    env: PolicyEnv,
    config: LifetimeConfig,
) -> (ShutdownPolicy, PolicySource) {
    // The window follows the same precedence, independently of the mode: a
    // config that sets `linger_seconds` is still describing the window when a
    // flag only overrides the mode.
    let window = |kind_source: PolicySource| -> u64 {
        flags
            .linger_seconds
            .or(env.linger_seconds)
            .or(config.linger_seconds)
            .unwrap_or_else(|| {
                let _ = kind_source;
                0
            })
    };

    if let Some(kind) = flags.shutdown {
        return (build(kind, window(PolicySource::Flag)), PolicySource::Flag);
    }
    if let Some(kind) = env.shutdown {
        return (build(kind, window(PolicySource::Env)), PolicySource::Env);
    }
    if config.is_unset() {
        return (ShutdownPolicy::OnLastDisconnect, PolicySource::Default);
    }
    (config.policy(), PolicySource::Config)
}

fn build(kind: ShutdownPolicyKind, linger_seconds: u64) -> ShutdownPolicy {
    match kind {
        ShutdownPolicyKind::OnLastDisconnect => ShutdownPolicy::OnLastDisconnect,
        ShutdownPolicyKind::Linger => ShutdownPolicy::Linger {
            seconds: linger_seconds,
        },
        ShutdownPolicyKind::Never => ShutdownPolicy::Never,
    }
}

/// Owns the lifetime decision and everything asynchronous about acting on it.
pub struct LifetimeSupervisor {
    state: Mutex<LifetimeState>,
    events: Arc<EventBus>,
    started_at: Instant,
    /// Signalled once, when the state commits. The accept loop waits on it.
    shutdown: Notify,
    /// Set with the commit so a waiter that arrives *after* the notify can still
    /// observe it — `Notify` alone drops a notification nobody is waiting on.
    committed: AtomicBool,
    /// Bumped every time a pending shutdown is armed. A linger timer carries the
    /// generation it was started for and does nothing if that generation is
    /// stale, which is how a returning client cancels a timer that is already
    /// sleeping (there is no way to cancel a `sleep` from outside).
    arm_generation: Mutex<u64>,
    /// Why the daemon is exiting, recorded by the first commit to win.
    exit_reason: Mutex<Option<ExitReason>>,
}

impl LifetimeSupervisor {
    /// A supervisor over a freshly bound daemon.
    #[must_use]
    pub fn new(policy: ShutdownPolicy, source: PolicySource, events: Arc<EventBus>) -> Self {
        Self {
            state: Mutex::new(LifetimeState::new(policy, source)),
            events,
            started_at: Instant::now(),
            shutdown: Notify::new(),
            committed: AtomicBool::new(false),
            arm_generation: Mutex::new(0),
            exit_reason: Mutex::new(None),
        }
    }

    /// Why the daemon is exiting, once it has committed.
    #[must_use]
    pub fn exit_reason(&self) -> Option<ExitReason> {
        *self.exit_reason.lock().expect("exit reason poisoned")
    }

    /// The effective policy, for the startup line.
    #[must_use]
    pub fn policy(&self) -> ShutdownPolicy {
        self.lock().policy()
    }

    /// Where the policy came from, for the startup line.
    #[must_use]
    pub fn source(&self) -> PolicySource {
        self.lock().source()
    }

    /// How long the daemon has been up.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Whether the daemon has committed to exiting.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed.load(Ordering::SeqCst)
    }

    /// Live client connections.
    #[must_use]
    pub fn client_count(&self) -> u32 {
        self.lock().client_count()
    }

    /// Where the daemon is in its lifetime.
    #[must_use]
    pub fn phase(&self) -> LifetimePhase {
        self.lock().phase()
    }

    /// Admit a client whose handshake just succeeded, or refuse it because the
    /// daemon has already committed to exiting (BR-3).
    ///
    /// The returned guard *is* the client's claim on the daemon's life; dropping
    /// it is the disconnect.
    pub fn admit(self: &Arc<Self>) -> Option<ClientGuard> {
        // One critical section covers the refusal check, the increment, and the
        // disarm. Splitting them would open exactly the window BR-3 closes.
        let (admission, action, count) = {
            let mut state = self.lock();
            let (admission, action) = state.admit();
            (admission, action, state.client_count())
        };
        if admission == Admission::Refused {
            return None;
        }
        self.apply(action);
        self.publish(DaemonLifetimeStage::ClientConnected {
            live_connection_count: count,
        });
        Some(ClientGuard {
            supervisor: Arc::clone(self),
        })
    }

    /// Claim `activity` as in flight; the daemon will not exit until the guard
    /// drops (BR-2).
    pub fn activity(self: &Arc<Self>, activity: BlockingActivity) -> ActivityGuard {
        self.lock().begin_activity(activity);
        ActivityGuard {
            supervisor: Arc::clone(self),
            activity,
        }
    }

    /// Resolves once the daemon has committed to exiting.
    ///
    /// The flag is checked *around* the creation of the `Notified` future, and
    /// the ordering is load-bearing: a commit landing between the two checks is
    /// still delivered, because tokio guarantees a `Notified` receives wakeups
    /// from **`notify_waiters()` as soon as it is created**, even before it is
    /// first polled.
    ///
    /// That guarantee is specific to `notify_waiters()`. `notify_one()` makes
    /// the weaker promise — an unpolled future may miss it — so switching
    /// [`Self::commit`] to `notify_one()` would silently turn this into a
    /// daemon that never learns it is supposed to exit. If that ever becomes
    /// necessary, this needs `tokio::pin!` + `Notified::enable()` before the
    /// second check.
    ///
    /// The `committed` flag is what makes a *late* waiter work at all:
    /// `notify_waiters()` wakes only those already waiting and keeps no permit,
    /// so a caller arriving after the commit is served by the flag, never by
    /// the notification.
    pub async fn wait_for_shutdown(&self) {
        loop {
            if self.is_committed() {
                return;
            }
            let notified = self.shutdown.notified();
            if self.is_committed() {
                return;
            }
            notified.await;
        }
    }

    /// Stop the daemon because it was signalled. Commits regardless of policy —
    /// [`ShutdownPolicy::Never`] governs *self*-termination only, which is what
    /// keeps `brew services stop` working under the always-on opt-in.
    pub fn signal(self: &Arc<Self>) {
        let action = self.lock().on_signal();
        self.apply(action);
    }

    /// Start the startup-grace timer. Call once, after the socket is bound.
    ///
    /// A no-op under a policy that never self-terminates, so an always-on daemon
    /// is not quietly holding a timer that would kill it.
    pub fn spawn_startup_grace(self: &Arc<Self>, grace: Duration) {
        if !self.policy().self_terminates() {
            return;
        }
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let action = supervisor.lock().on_startup_grace_elapsed();
            supervisor.apply(action);
        });
    }

    /// Apply one [`LifetimeAction`]: publish what it implies, start or retire a
    /// timer, or commit.
    fn apply(self: &Arc<Self>, action: LifetimeAction) {
        match action {
            LifetimeAction::None => {}
            LifetimeAction::Disarm => {
                // Retire any sleeping linger timer by moving the generation past
                // the one it captured.
                *self.arm_generation.lock().expect("arm generation poisoned") += 1;
            }
            LifetimeAction::Arm { linger_seconds } => {
                let generation = {
                    let mut gen = self.arm_generation.lock().expect("arm generation poisoned");
                    *gen += 1;
                    *gen
                };
                self.publish(DaemonLifetimeStage::ShutdownArmed {
                    policy: policy_label(self.policy()),
                    linger_seconds,
                });
                let supervisor = Arc::clone(self);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(linger_seconds)).await;
                    // A client that returned during the sleep bumped the
                    // generation; this timer is then stale and must not exit a
                    // daemon that is serving again.
                    let current = *supervisor
                        .arm_generation
                        .lock()
                        .expect("arm generation poisoned");
                    if current != generation {
                        return;
                    }
                    let action = supervisor.lock().on_linger_elapsed();
                    supervisor.apply(action);
                });
            }
            LifetimeAction::Defer { blocking_activity } => {
                self.publish(DaemonLifetimeStage::ShutdownDeferred { blocking_activity });
            }
            LifetimeAction::Commit { reason } => self.commit(reason),
        }
    }

    /// Record the decision to exit and wake the accept loop.
    ///
    /// Idempotent: a signal racing a last disconnect must not fire two
    /// shutdowns, and only the first reason is reported.
    fn commit(&self, reason: ExitReason) {
        if self.committed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.exit_reason
            .lock()
            .expect("exit reason poisoned")
            .replace(reason);
        self.shutdown.notify_waiters();
    }

    /// Publish one lifetime stage to attached clients and the daemon log.
    ///
    /// The log line is what the acceptance suite asserts on: the shutdown stages
    /// fire when no client is attached (the count is already zero), so the bus
    /// has no audience for them and stderr is the only place they can be
    /// observed.
    fn publish(&self, stage: DaemonLifetimeStage) {
        eprintln!("teton-code: {}", describe(&stage));
        self.events
            .publish(None, Event::DaemonLifetime(DaemonLifetime { stage }));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LifetimeState> {
        self.state.lock().expect("lifetime state mutex poisoned")
    }
}

/// The consent gate's view of the supervisor: "an install is writing bytes".
///
/// Claimed as [`BlockingActivity::ModelDownload`] because that is what the
/// claim protects — the download/verify/load span ADR-006 treats as one unit of
/// work. It deliberately does *not* cover the consent flow's wait for a user
/// decision: waiting for a human is not in-flight work, and a claim held across
/// it would mean an unanswered proposal pins the daemon forever, which is the
/// standing-resident-daemon harm REQ-565 exists to remove.
/// Adapts the supervisor to the consent gate's [`WorkClaim`] seam (REQ-565).
///
/// A newtype rather than an `impl` on the supervisor itself, because an
/// [`ActivityGuard`] must own an `Arc<LifetimeSupervisor>` for its `Drop` to
/// reach back, and the trait method only gets `&self`. Holding the handle here
/// makes that requirement explicit instead of hiding it behind a double `Arc`.
pub struct LifetimeWorkClaim(Arc<LifetimeSupervisor>);

impl LifetimeWorkClaim {
    /// Wrap a supervisor so the consent gate can claim against it.
    #[must_use]
    pub fn new(supervisor: Arc<LifetimeSupervisor>) -> Self {
        Self(supervisor)
    }
}

impl crate::model_consent::WorkClaim for LifetimeWorkClaim {
    /// Claimed as [`BlockingActivity::ModelDownload`] — the download → verify →
    /// load span ADR-006 already treats as one unit of work. It covers the
    /// bytes being written, never the consent flow's wait for a human: a claim
    /// held across an unanswered proposal would pin the daemon forever.
    fn claim(&self) -> Box<dyn Send> {
        Box::new(self.0.activity(BlockingActivity::ModelDownload))
    }
}

/// A client's claim on the daemon's life. Dropping it is the disconnect.
pub struct ClientGuard {
    supervisor: Arc<LifetimeSupervisor>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let (action, count) = {
            let mut state = self.supervisor.lock();
            let action = state.on_disconnect();
            (action, state.client_count())
        };
        self.supervisor
            .publish(DaemonLifetimeStage::ClientDisconnected {
                live_connection_count: count,
            });
        self.supervisor.apply(action);
    }
}

/// A claim that in-flight work is happening. Dropping it may commit a pending
/// shutdown (BR-2).
pub struct ActivityGuard {
    supervisor: Arc<LifetimeSupervisor>,
    activity: BlockingActivity,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let action = self.supervisor.lock().end_activity(self.activity);
        self.supervisor.apply(action);
    }
}

/// The policy name used in the `shutdown_armed` payload and the startup line.
#[must_use]
pub fn policy_label(policy: ShutdownPolicy) -> String {
    match policy {
        ShutdownPolicy::OnLastDisconnect => "on-last-disconnect".to_owned(),
        ShutdownPolicy::Linger { seconds } => format!("linger({seconds}s)"),
        ShutdownPolicy::Never => "never".to_owned(),
    }
}

/// One human-readable line per stage, for the daemon log.
fn describe(stage: &DaemonLifetimeStage) -> String {
    match stage {
        DaemonLifetimeStage::ClientConnected {
            live_connection_count,
        } => format!("client_connected (live_connection_count={live_connection_count})"),
        DaemonLifetimeStage::ClientDisconnected {
            live_connection_count,
        } => format!("client_disconnected (live_connection_count={live_connection_count})"),
        DaemonLifetimeStage::ShutdownArmed {
            policy,
            linger_seconds,
        } => format!("daemon_shutdown_armed (policy={policy}, linger_seconds={linger_seconds})"),
        DaemonLifetimeStage::ShutdownDeferred { blocking_activity } => {
            let activity = serde_json::to_string(blocking_activity)
                .unwrap_or_else(|_| "\"unknown\"".to_owned());
            format!("daemon_shutdown_deferred (blocking_activity={activity})")
        }
        DaemonLifetimeStage::Shutdown {
            reason,
            uptime_seconds,
            sessions_closed,
        } => {
            let reason = serde_json::to_string(reason).unwrap_or_else(|_| "\"unknown\"".to_owned());
            format!(
                "daemon_shutdown (reason={reason}, uptime_seconds={uptime_seconds}, \
                 sessions_closed={sessions_closed})"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Flag parsing (BR-7)
    // -----------------------------------------------------------------------

    #[test]
    fn both_flag_spellings_parse() {
        // launchd plists and hand-typed command lines disagree about which of
        // these they use, so the daemon has to accept both.
        let spaced = parse_policy_flags(["--shutdown-policy", "never"]).unwrap();
        let inline = parse_policy_flags(["--shutdown-policy=never"]).unwrap();
        assert_eq!(spaced.shutdown, Some(ShutdownPolicyKind::Never));
        assert_eq!(inline.shutdown, spaced.shutdown);
    }

    #[test]
    fn the_linger_window_parses_as_a_number() {
        let flags = parse_policy_flags(["--linger-seconds", "45"]).unwrap();
        assert_eq!(flags.linger_seconds, Some(45));
    }

    /// The failure mode this refusal exists for: a typo that silently fell back
    /// to the default would hand the `brew services` daemon exit-on-last-client,
    /// and it would flap against launchd's keep-alive (BR-5).
    #[test]
    fn an_unknown_mode_refuses_and_names_the_valid_spellings() {
        let err = parse_policy_flags(["--shutdown-policy", "nevr"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nevr"), "the typo must be visible: {msg}");
        for spelling in ShutdownPolicyKind::SPELLINGS {
            assert!(msg.contains(spelling), "`{spelling}` missing from: {msg}");
        }
    }

    #[test]
    fn a_non_numeric_window_and_a_valueless_flag_both_refuse() {
        assert!(matches!(
            parse_policy_flags(["--linger-seconds", "soon"]).unwrap_err(),
            PolicyArgError::NotANumber { .. }
        ));
        assert!(matches!(
            parse_policy_flags(["--shutdown-policy"]).unwrap_err(),
            PolicyArgError::MissingValue { .. }
        ));
    }

    #[test]
    fn unrelated_arguments_are_ignored() {
        let flags = parse_policy_flags(["--version", "-V", "--not-ours=1"]).unwrap();
        assert_eq!(flags, PolicyFlags::default());
    }

    // -----------------------------------------------------------------------
    // Precedence (BR-7, D-7)
    // -----------------------------------------------------------------------

    fn config(kind: ShutdownPolicyKind, linger: Option<u64>) -> LifetimeConfig {
        LifetimeConfig {
            shutdown: kind,
            linger_seconds: linger,
        }
    }

    #[test]
    fn nothing_set_anywhere_is_exit_with_the_last_client() {
        let (policy, source) = resolve_policy(
            PolicyFlags::default(),
            PolicyEnv::default(),
            LifetimeConfig::default(),
        );
        assert_eq!(policy, ShutdownPolicy::OnLastDisconnect);
        assert_eq!(source, PolicySource::Default);
    }

    #[test]
    fn the_flag_outranks_the_environment_and_the_config() {
        let (policy, source) = resolve_policy(
            PolicyFlags {
                shutdown: Some(ShutdownPolicyKind::Never),
                linger_seconds: None,
            },
            PolicyEnv {
                shutdown: Some(ShutdownPolicyKind::Linger),
                linger_seconds: Some(10),
            },
            config(ShutdownPolicyKind::OnLastDisconnect, None),
        );
        assert_eq!(policy, ShutdownPolicy::Never);
        assert_eq!(source, PolicySource::Flag);
    }

    #[test]
    fn the_environment_outranks_the_config() {
        let (policy, source) = resolve_policy(
            PolicyFlags::default(),
            PolicyEnv {
                shutdown: Some(ShutdownPolicyKind::Never),
                linger_seconds: None,
            },
            config(ShutdownPolicyKind::OnLastDisconnect, None),
        );
        assert_eq!(policy, ShutdownPolicy::Never);
        assert_eq!(source, PolicySource::Env);
    }

    #[test]
    fn a_config_that_sets_a_policy_is_reported_as_the_source() {
        let (policy, source) = resolve_policy(
            PolicyFlags::default(),
            PolicyEnv::default(),
            config(ShutdownPolicyKind::Linger, Some(30)),
        );
        assert_eq!(policy, ShutdownPolicy::Linger { seconds: 30 });
        assert_eq!(source, PolicySource::Config);
    }

    /// The window follows the same precedence as the mode but independently, so
    /// a flag that only names `linger` still honours a window the config set.
    #[test]
    fn the_window_resolves_independently_of_the_mode() {
        let (policy, source) = resolve_policy(
            PolicyFlags {
                shutdown: Some(ShutdownPolicyKind::Linger),
                linger_seconds: None,
            },
            PolicyEnv::default(),
            config(ShutdownPolicyKind::Linger, Some(90)),
        );
        assert_eq!(policy, ShutdownPolicy::Linger { seconds: 90 });
        assert_eq!(source, PolicySource::Flag);
    }

    /// The Homebrew service block's exact invocation. If this ever stops
    /// yielding `Never`, the shipped always-on daemon exits under launchd's
    /// keep-alive and flaps — reloading the model every cycle (BR-5).
    #[test]
    fn the_shipped_service_invocation_resolves_to_never() {
        let flags = parse_policy_flags(["--shutdown-policy", "never"]).unwrap();
        let (policy, source) =
            resolve_policy(flags, PolicyEnv::default(), LifetimeConfig::default());
        assert_eq!(policy, ShutdownPolicy::Never);
        assert_eq!(source, PolicySource::Flag);
        assert!(!policy.self_terminates());
    }

    #[test]
    fn policy_labels_name_the_mode_and_its_window() {
        assert_eq!(
            policy_label(ShutdownPolicy::OnLastDisconnect),
            "on-last-disconnect"
        );
        assert_eq!(policy_label(ShutdownPolicy::Never), "never");
        assert_eq!(
            policy_label(ShutdownPolicy::Linger { seconds: 30 }),
            "linger(30s)"
        );
    }
}
