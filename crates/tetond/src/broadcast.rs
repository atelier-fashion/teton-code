//! The event bus: fan-out of daemon events to subscribed clients.
//!
//! ADR-002 deferred the backpressure policy to this task. The rule implemented
//! here: every subscriber gets its own **bounded** channel. On publish the
//! daemon `try_send`s to each subscriber and never blocks. If a subscriber's
//! channel is full — a client too slow to drain its stream — that subscription
//! is evicted on the spot and flagged as lagged; the client's forwarder then
//! sends it a [`teton_protocol::events::SUBSCRIPTION_LAGGED_METHOD`] notice and
//! closes. A slow client can thus never buffer unboundedly nor stall the
//! publisher or its peers.
//!
//! The wire vocabulary that notice is built from — the method name and its
//! [`teton_protocol::jsonrpc::error_code::SUBSCRIPTION_LAGGED`] code — lives in
//! the protocol crate, where the client reads it from the same declaration.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use teton_protocol::events::{Event, EventEnvelope};
use teton_protocol::SessionId;

/// Default per-subscriber channel depth. Large enough to absorb normal bursts,
/// small enough that a truly stuck client is evicted promptly.
pub const DEFAULT_CAPACITY: usize = 256;

/// An observer of every **session-scoped** envelope, called on the publish path
/// (REQ-611 ADR-1).
///
/// # Not a subscriber, and the difference is the whole point
///
/// A subscriber whose channel fills is evicted and never re-admitted — the
/// right contract for a client and the wrong one for a record (LESSON-513). A
/// tap cannot be evicted: it is offered every envelope for as long as it is
/// installed, and what it does with one it cannot take is its own business to
/// count. That is what lets the transcript promise "never a silent hole"
/// (REQ-611 BR-5).
///
/// # `observe` runs under the bus mutex, so it must not block
///
/// [`EventBus::publish`] calls this with the lock held and before the
/// subscriber fan-out, which is safe **only** because the implementation
/// cannot wait. The signature is the enforcement: no return value, so there is
/// nothing for a caller to await or handle, and the one shipped implementation
/// (`crate::transcript::TranscriptSink`) is a `try_send` and nothing else. An
/// implementation that acquired a lock, allocated unboundedly, logged, or —
/// the mutation `the_tap_never_blocks_publish_and_counts_its_drops` is written
/// against — did a blocking send would stall every publisher in the daemon
/// (LESSON-518).
///
/// Only envelopes carrying a `session_id` are offered (REQ-611 BR-7): a
/// daemon-scoped envelope belongs to no session, and a file that held one would
/// be putting other sessions' activity into a record its owner may share.
pub trait EventTap: Send + Sync {
    /// Observe one session-scoped envelope, in its published wire form.
    ///
    /// Called synchronously under the bus lock. Never blocks, never fails.
    fn observe(&self, envelope: &EventEnvelope);
}

/// One registered subscriber, held by the bus.
struct SubscriberHandle {
    id: u64,
    tx: mpsc::Sender<EventEnvelope>,
    lagged: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
}

struct Inner {
    next_id: u64,
    seq: u64,
    subscribers: Vec<SubscriberHandle>,
    /// The installed tap (REQ-611 ADR-1), offered every session-scoped envelope
    /// before the fan-out below it.
    ///
    /// Inside `Inner` rather than beside it so that `publish` reads it under the
    /// lock it already holds: a second mutex on the publish path would be a
    /// second thing to acquire per event, and a lock-free slot read would let a
    /// tap installed between the mint and the fan-out see an envelope whose
    /// predecessor it never saw.
    tap: Option<Arc<dyn EventTap>>,
}

/// A many-subscriber event fan-out with bounded, non-blocking delivery.
pub struct EventBus {
    inner: Mutex<Inner>,
}

impl EventBus {
    /// A bus with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 0,
                seq: 0,
                subscribers: Vec::new(),
                tap: None,
            }),
        }
    }

    /// Install the observer every session-scoped envelope is offered to
    /// (REQ-611 ADR-1).
    ///
    /// One tap, last writer wins. The daemon installs exactly one — the
    /// transcript sink, from `DaemonRuntime::from_env` — and a second consumer
    /// of every envelope is a subscriber, which is what `subscribe` is for. A
    /// list here would invite a caller to add a slow observer to a path whose
    /// whole contract is that nothing on it waits.
    pub fn install_tap(&self, tap: Arc<dyn EventTap>) {
        self.inner.lock().expect("event bus mutex poisoned").tap = Some(tap);
    }

    /// Registers a subscriber with a bounded channel of `capacity` events.
    pub fn subscribe(self: &Arc<Self>, capacity: usize) -> Subscription {
        let (tx, rx) = mpsc::channel(capacity);
        let lagged = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(AtomicU64::new(0));

        let id = {
            let mut inner = self.inner.lock().expect("event bus mutex poisoned");
            let id = inner.next_id;
            inner.next_id += 1;
            inner.subscribers.push(SubscriberHandle {
                id,
                tx,
                lagged: Arc::clone(&lagged),
                delivered: Arc::clone(&delivered),
            });
            id
        };

        Subscription {
            id,
            rx,
            lagged,
            delivered,
            bus: Arc::clone(self),
        }
    }

    /// Publishes `event` to every current subscriber.
    ///
    /// Assigns the next broadcast sequence number, wraps the event in an
    /// [`EventEnvelope`], and `try_send`s a clone to each subscriber. Full
    /// subscribers are flagged lagged and dropped; already-closed subscribers
    /// are pruned. This call never blocks and never awaits.
    pub fn publish(&self, session_id: Option<SessionId>, event: Event) {
        let mut inner = self.inner.lock().expect("event bus mutex poisoned");
        let seq = inner.seq;
        inner.seq += 1;
        let envelope = EventEnvelope::new(seq, session_id, event);

        // REQ-611 ADR-1: the tap sees the envelope **before** the fan-out, with
        // the `seq` already minted, so a transcript's bus-sourced records are
        // the wire form verbatim. Before rather than after because the retain
        // below consumes the subscriber list and can evict — an observer placed
        // after it would be reading a path whose failures it must not share.
        //
        // REQ-611 BR-7: session-scoped only. A daemon-scoped envelope belongs to
        // no session's file, and this is the seam that knows which it is.
        if envelope.session_id.is_some() {
            if let Some(tap) = inner.tap.as_ref() {
                tap.observe(&envelope);
            }
        }

        inner.subscribers.retain(|handle| {
            match handle.tx.try_send(envelope.clone()) {
                Ok(()) => {
                    // Delivery count, not publish count: an event this
                    // subscriber never received (published pre-subscribe, or
                    // dropped on eviction) must not be waited for by anyone
                    // holding the counter (see `Subscription::delivered_counter`).
                    handle.delivered.fetch_add(1, Ordering::SeqCst);
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Slow client: evict rather than buffer unboundedly. The
                    // flag lets the client's forwarder emit a lagged notice
                    // once it drains what it has and sees the channel close.
                    handle.lagged.store(true, Ordering::SeqCst);
                    false
                }
                // Receiver already gone (client disconnected): prune silently.
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// Reserves the next sequence number without publishing anything.
    ///
    /// For the delivery paths that are not a fan-out: REQ-569's consent frames
    /// are *routed* to named connections (BR-6) rather than broadcast, and so
    /// is the handshake's lifecycle replay, which is one connection's catch-up
    /// (BUG-177). The caller builds and sends the envelope itself — but the
    /// number on it has to come from this counter, or a routed frame and a
    /// broadcast one could reach the same client wearing the same `seq`.
    ///
    /// Reserving a number that no subscriber receives leaves a gap in every
    /// other client's sequence, which is already the norm after REQ-568: the
    /// counter is bus-wide while delivery is filtered per connection, so a
    /// client sees only the subsequence it was entitled to.
    pub fn next_seq(&self) -> u64 {
        let mut inner = self.inner.lock().expect("event bus mutex poisoned");
        let seq = inner.seq;
        inner.seq += 1;
        seq
    }

    /// The number the **next** publish will mint, without minting it.
    ///
    /// [`Self::next_seq`]'s read-only twin, and the distinction matters: that
    /// one *reserves*, which leaves a gap in every client's sequence, and is
    /// right for a routed frame that will wear the number. This one is for a
    /// caller that wants to say *where in the numbering it started* — REQ-611's
    /// `transcript_opened` records it so a reader of a file knows which part of
    /// the daemon-wide stream it covers. Burning a sequence number to answer
    /// that would put a hole in every attached client's stream for a fact no
    /// client ever sees.
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.inner.lock().expect("event bus mutex poisoned").seq
    }

    /// Number of currently registered subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .expect("event bus mutex poisoned")
            .subscribers
            .len()
    }

    /// Removes a subscriber by id (idempotent).
    fn remove(&self, id: u64) {
        self.inner
            .lock()
            .expect("event bus mutex poisoned")
            .subscribers
            .retain(|handle| handle.id != id);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// A client's handle onto the event stream.
///
/// Dropping it unregisters the subscriber from the bus, so a disconnecting
/// client leaves no dangling channel behind.
pub struct Subscription {
    id: u64,
    rx: mpsc::Receiver<EventEnvelope>,
    lagged: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
    bus: Arc<EventBus>,
}

impl Subscription {
    /// Awaits the next event, or `None` when the subscription has ended (the
    /// client disconnected, or the bus evicted it for lagging — distinguish
    /// the two with [`Subscription::is_lagged`]).
    pub async fn recv(&mut self) -> Option<EventEnvelope> {
        self.rx.recv().await
    }

    /// The next already-queued event, or `None` when the queue is empty right
    /// now. Never waits: `EventBus::publish` is synchronous, so a caller that
    /// knows publishing has finished can drain deterministically instead of
    /// polling `recv` under a wall-clock timeout (the assertion shape that
    /// goes flaky first under CI scheduler pressure — LESSON-450).
    ///
    /// `None` conflates "empty right now" with "ended" — use [`Self::recv`]
    /// when that distinction matters, and [`Self::is_lagged`] to tell an
    /// eviction from a disconnect. A caller that loops on this without an
    /// end condition of its own would busy-spin on a live, empty queue.
    pub fn try_recv(&mut self) -> Option<EventEnvelope> {
        self.rx.try_recv().ok()
    }

    /// Whether the bus evicted this subscription for falling behind.
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        self.lagged.load(Ordering::SeqCst)
    }

    /// The live count of events the bus has queued into this subscription's
    /// channel so far. `publish` increments it synchronously (under the bus
    /// lock, after a successful delivery), so once a publisher returns, a load
    /// of this counter covers everything it just published.
    ///
    /// This is the event side of the server's event/response ordering fence:
    /// a task that drains this subscription counts what it has *taken out*, and
    /// a response is held until that count catches up to what was put in (see
    /// `server::EventFence`). Kept as a shared handle so the counter stays
    /// readable after the `Subscription` itself moves into the drain task.
    #[must_use]
    pub fn delivered_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.delivered)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use teton_protocol::events::{
        DaemonClientAttach, ModelLifecycle, ModelLifecycleStage, SessionUpdate,
        SessionUpdatePayload,
    };
    use teton_protocol::{ClientKind, PROTOCOL_VERSION};

    fn an_event() -> Event {
        Event::DaemonClientAttach(DaemonClientAttach {
            client_kind: ClientKind::Cli,
            protocol_version: PROTOCOL_VERSION,
        })
    }

    /// A daemon-scoped event by nature: the local tier's lifecycle belongs to
    /// the machine, not to any session (REQ-611 BR-7 names it).
    fn a_daemon_scoped_event() -> Event {
        Event::ModelLifecycle(ModelLifecycle {
            model_id: "on-device".to_owned(),
            stage: ModelLifecycleStage::Probed {
                ram_bytes: 0,
                above_floor: false,
            },
        })
    }

    /// A session-scoped event by nature: streamed turn text.
    fn a_session_scoped_event(text: &str) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::AgentMessageChunk {
                text: text.to_owned(),
            },
        })
    }

    /// A tap with the shipped sink's contract in miniature: a bounded channel,
    /// `try_send` only, and a count of what it could not take (REQ-611 ADR-1).
    ///
    /// Stands in for `crate::transcript::TranscriptSink` deliberately. What
    /// these tests assert is a property of the **bus** — that a tap which
    /// cannot take an envelope costs the publisher nothing — so the double is
    /// the smallest thing that can be full, and the sink's own drop accounting
    /// is tested where it lives.
    struct CountingTap {
        tx: mpsc::Sender<EventEnvelope>,
        taken: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
    }

    /// What [`CountingTap::holding`] hands back: the tap, the receiver the
    /// caller must keep alive, and the two counters.
    ///
    /// The receiver is returned rather than kept inside the tap because a
    /// `Receiver` is not `Sync` and a tap must be — and it must be *held*
    /// rather than dropped, because a closed channel fails `try_send` as
    /// `Closed`, which would make every publish look full for the wrong reason.
    type TapFixture = (
        Arc<CountingTap>,
        mpsc::Receiver<EventEnvelope>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    );

    impl CountingTap {
        /// A tap whose channel holds `capacity` envelopes and is never drained.
        fn holding(capacity: usize) -> TapFixture {
            let (tx, rx) = mpsc::channel(capacity);
            let taken = Arc::new(AtomicU64::new(0));
            let dropped = Arc::new(AtomicU64::new(0));
            let tap = Arc::new(Self {
                tx,
                taken: Arc::clone(&taken),
                dropped: Arc::clone(&dropped),
            });
            (tap, rx, taken, dropped)
        }
    }

    impl EventTap for CountingTap {
        fn observe(&self, envelope: &EventEnvelope) {
            // THE MUTATION for `the_tap_never_blocks_publish_and_counts_its_drops`
            // is this line as `self.tx.blocking_send(envelope.clone())`.
            match self.tx.try_send(envelope.clone()) {
                Ok(()) => self.taken.fetch_add(1, Ordering::SeqCst),
                Err(_) => self.dropped.fetch_add(1, Ordering::SeqCst),
            };
        }
    }

    /// **REQ-611 BR-5, ADR-1: a full tap never delays a publish, and the
    /// shortfall is counted rather than lost.**
    ///
    /// The tap's channel holds exactly one envelope and nothing drains it, so
    /// 99 of the 100 publishes below meet a full channel. Three things must all
    /// hold: every `publish` returns, the bus's ordinary subscriber still
    /// receives all 100, and `taken + dropped` is 100 — a hole the tap knows
    /// the size of, which is what a transcript turns into one `transcript_gap`
    /// record instead of a silent gap in the file (LESSON-513).
    ///
    /// **Mutation (run, red):** `CountingTap::observe`'s `try_send` →
    /// `blocking_send`. The test does not merely fail — the process aborts.
    /// The first full send panics with *"Cannot block the current thread from
    /// within a runtime"*, and because it panics **while holding the bus
    /// mutex** the next publisher meets *"event bus mutex poisoned"* and the
    /// run ends in `SIGABRT`. That cascade is the whole argument for the
    /// signature: a tap that waits does not slow one publisher down, it takes
    /// the bus with it (LESSON-518). Restored.
    #[tokio::test]
    async fn the_tap_never_blocks_publish_and_counts_its_drops() {
        let bus = Arc::new(EventBus::new());
        let (tap, _rx, taken, dropped) = CountingTap::holding(1);
        bus.install_tap(tap as Arc<dyn EventTap>);
        let mut client = bus.subscribe(256);

        let session = SessionId::from("s1");
        for n in 0..100 {
            bus.publish(
                Some(session.clone()),
                a_session_scoped_event(&format!("chunk {n}")),
            );
        }

        // The publisher was never held up: reaching this line at all is the
        // claim, and the counts below say the tap really was full while it ran.
        assert_eq!(taken.load(Ordering::SeqCst), 1, "the channel holds one");
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            99,
            "every envelope the tap could not take is counted"
        );
        assert_eq!(
            taken.load(Ordering::SeqCst) + dropped.load(Ordering::SeqCst),
            100,
            "counted + taken accounts for every publish: no silent hole"
        );

        // The bus's own audience is untouched by any of it.
        let mut received = 0;
        while client.try_recv().is_some() {
            received += 1;
        }
        assert_eq!(received, 100, "the subscriber still got every envelope");
    }

    /// **REQ-611 BR-7: a daemon-scoped envelope is never offered to the tap.**
    ///
    /// `model_lifecycle` belongs to the machine, not to a session, and a
    /// transcript that recorded one would be putting another session's activity
    /// — or none at all — into a file its owner may share. The zero is paired
    /// with a one on the same instrument (LESSON-479): the session-scoped
    /// envelope published afterwards *is* observed, so the tap was installed,
    /// live, and capable of counting the whole time.
    ///
    /// **Mutation (run, red):** drop the `envelope.session_id.is_some()` guard
    /// in `publish` — `taken` becomes 2 and the first assertion fails. Restored.
    #[tokio::test]
    async fn daemon_scoped_envelopes_are_not_offered_to_the_tap() {
        let bus = Arc::new(EventBus::new());
        let (tap, _rx, taken, dropped) = CountingTap::holding(64);
        bus.install_tap(tap as Arc<dyn EventTap>);

        bus.publish(None, a_daemon_scoped_event());
        assert_eq!(
            taken.load(Ordering::SeqCst),
            0,
            "a daemon-scoped envelope reaches no transcript"
        );
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "and it is not counted as a drop either — it was never offered"
        );

        // The positive control, on the same tap.
        bus.publish(Some(SessionId::from("s1")), a_session_scoped_event("hello"));
        assert_eq!(
            taken.load(Ordering::SeqCst),
            1,
            "a session-scoped envelope is offered"
        );
    }

    /// **REQ-611 ADR-1: evicting a lagging subscriber costs the tap nothing.**
    ///
    /// The reason the sink is a tap and not a subscriber. The slow client's
    /// channel fills at 2 and it is evicted on the third publish; the tap keeps
    /// being offered every envelope after that, because the eviction acts on
    /// the subscriber list the tap is not in.
    #[tokio::test]
    async fn a_subscriber_evicted_for_lag_does_not_cost_the_tap_an_envelope() {
        let bus = Arc::new(EventBus::new());
        let (tap, _rx, taken, _dropped) = CountingTap::holding(64);
        bus.install_tap(tap as Arc<dyn EventTap>);
        let slow = bus.subscribe(2); // deliberately never drained

        let session = SessionId::from("s1");
        for n in 0..10 {
            bus.publish(
                Some(session.clone()),
                a_session_scoped_event(&format!("chunk {n}")),
            );
        }

        assert!(slow.is_lagged(), "the subscriber was evicted");
        assert_eq!(bus.subscriber_count(), 0);
        assert_eq!(
            taken.load(Ordering::SeqCst),
            10,
            "the tap observed every envelope, including those published after \
             the eviction"
        );
    }

    /// `current_seq` reports the next number without spending it — the
    /// distinction `transcript_opened` depends on (REQ-611).
    #[tokio::test]
    async fn peeking_the_sequence_does_not_mint_one() {
        let bus = Arc::new(EventBus::new());
        let mut client = bus.subscribe(8);

        bus.publish(Some(SessionId::from("s1")), an_event());
        let peeked = bus.current_seq();
        assert_eq!(peeked, 1, "one publish has happened");
        assert_eq!(bus.current_seq(), peeked, "peeking twice reads the same");

        bus.publish(Some(SessionId::from("s1")), an_event());
        let first = client.recv().await.unwrap();
        let second = client.recv().await.unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(
            second.seq, peeked,
            "the peeked number is the one the next publish wore: no gap"
        );
    }

    #[tokio::test]
    async fn a_published_event_reaches_every_subscriber() {
        let bus = Arc::new(EventBus::new());
        let mut a = bus.subscribe(8);
        let mut b = bus.subscribe(8);

        bus.publish(Some(SessionId::from("s1")), an_event());

        let ea = a.recv().await.unwrap();
        let eb = b.recv().await.unwrap();
        assert_eq!(ea.session_id, Some(SessionId::from("s1")));
        assert_eq!(eb.session_id, Some(SessionId::from("s1")));
        assert_eq!(ea.event_name(), "daemon_client_attach");
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_evicted_without_blocking_the_publisher_or_peers() {
        let bus = Arc::new(EventBus::new());
        let slow = bus.subscribe(2); // deliberately never drained
        let mut fast = bus.subscribe(64);
        assert_eq!(bus.subscriber_count(), 2);

        // Publish well past the slow channel's depth. `publish` is synchronous
        // and non-blocking, so simply returning proves it did not stall.
        for _ in 0..10 {
            bus.publish(None, an_event());
        }

        // The slow subscriber was evicted and flagged; only the fast one remains.
        assert!(slow.is_lagged());
        assert_eq!(bus.subscriber_count(), 1);

        // The healthy subscriber still received every event.
        let mut received = 0;
        while received < 10 {
            match tokio::time::timeout(Duration::from_millis(100), fast.recv()).await {
                Ok(Some(_)) => received += 1,
                _ => break,
            }
        }
        assert_eq!(received, 10);
    }

    #[tokio::test]
    async fn dropping_a_subscription_unregisters_it() {
        let bus = Arc::new(EventBus::new());
        let a = bus.subscribe(4);
        assert_eq!(bus.subscriber_count(), 1);
        drop(a);
        assert_eq!(bus.subscriber_count(), 0);
    }

    // The lagged code's non-collision is no longer guarded here. It lives in
    // `teton_protocol::jsonrpc::error_code` with every other application code,
    // and `every_application_error_code_is_distinct` there checks the whole set
    // structurally — including codes added after this comment was written.
}
