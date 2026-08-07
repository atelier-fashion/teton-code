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
            }),
        }
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
    use teton_protocol::events::DaemonClientAttach;
    use teton_protocol::{ClientKind, PROTOCOL_VERSION};

    fn an_event() -> Event {
        Event::DaemonClientAttach(DaemonClientAttach {
            client_kind: ClientKind::Cli,
            protocol_version: PROTOCOL_VERSION,
        })
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
