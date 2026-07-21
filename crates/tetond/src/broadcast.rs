//! The event bus: fan-out of daemon events to subscribed clients.
//!
//! ADR-002 deferred the backpressure policy to this task. The rule implemented
//! here: every subscriber gets its own **bounded** channel. On publish the
//! daemon `try_send`s to each subscriber and never blocks. If a subscriber's
//! channel is full — a client too slow to drain its stream — that subscription
//! is evicted on the spot and flagged as lagged; the client's forwarder then
//! sends it a [`SUBSCRIPTION_LAGGED_METHOD`] notice and closes. A slow client
//! can thus never buffer unboundedly nor stall the publisher or its peers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use teton_protocol::events::{Event, EventEnvelope};
use teton_protocol::SessionId;

/// Default per-subscriber channel depth. Large enough to absorb normal bursts,
/// small enough that a truly stuck client is evicted promptly.
pub const DEFAULT_CAPACITY: usize = 256;

/// Application error code for an evicted (lagged) subscription.
///
/// Sits just past the protocol's existing application codes
/// (`teton_protocol::jsonrpc::error_code`, which end at `-32003`) without
/// colliding with any of them.
pub const SUBSCRIPTION_LAGGED_CODE: i64 = -32004;

/// JSON-RPC notification method the daemon sends before dropping a lagged
/// subscription.
pub const SUBSCRIPTION_LAGGED_METHOD: &str = "subscription/lagged";

/// One registered subscriber, held by the bus.
struct SubscriberHandle {
    id: u64,
    tx: mpsc::Sender<EventEnvelope>,
    lagged: Arc<AtomicBool>,
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

        let id = {
            let mut inner = self.inner.lock().expect("event bus mutex poisoned");
            let id = inner.next_id;
            inner.next_id += 1;
            inner.subscribers.push(SubscriberHandle {
                id,
                tx,
                lagged: Arc::clone(&lagged),
            });
            id
        };

        Subscription {
            id,
            rx,
            lagged,
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
                Ok(()) => true,
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
    bus: Arc<EventBus>,
}

impl Subscription {
    /// Awaits the next event, or `None` when the subscription has ended (the
    /// client disconnected, or the bus evicted it for lagging — distinguish
    /// the two with [`Subscription::is_lagged`]).
    pub async fn recv(&mut self) -> Option<EventEnvelope> {
        self.rx.recv().await
    }

    /// Whether the bus evicted this subscription for falling behind.
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        self.lagged.load(Ordering::SeqCst)
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

    #[test]
    fn lagged_code_does_not_collide_with_protocol_error_codes() {
        use teton_protocol::jsonrpc::error_code;
        for code in [
            error_code::UNSUPPORTED_PROTOCOL_VERSION,
            error_code::UNKNOWN_SESSION,
            error_code::UNKNOWN_PROVIDER,
            error_code::CONFIG_REJECTED,
        ] {
            assert_ne!(code, SUBSCRIPTION_LAGGED_CODE);
        }
    }
}
