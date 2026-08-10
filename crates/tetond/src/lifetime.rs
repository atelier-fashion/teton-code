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
    /// Checks the flag before and after registering interest so a commit that
    /// lands between the two cannot be missed.
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
