//! The event bus itself.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::broadcast;

use super::{Envelope, Event, EventMetadata, EventStream};
use crate::clock::{SharedClock, SystemClock};
use crate::id::EventName;

/// The default per-event-type channel capacity.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// A typed publish/subscribe bus.
///
/// Cloning is cheap and yields a handle to the same bus.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

struct Inner {
    capacity: usize,
    clock: SharedClock,
    /// One `broadcast::Sender<Envelope<E>>` per event type, type-erased.
    channels: RwLock<HashMap<TypeId, Box<dyn std::any::Any + Send + Sync>>>,
    /// The JSON firehose, carrying every event regardless of type.
    firehose: broadcast::Sender<Envelope<Value>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let channels = self.inner.channels.read().expect("event bus lock poisoned");
        f.debug_struct("EventBus")
            .field("capacity", &self.inner.capacity)
            .field("event_types", &channels.len())
            .field("firehose_subscribers", &self.inner.firehose.receiver_count())
            .finish()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY, Arc::new(SystemClock))
    }
}

impl EventBus {
    /// Creates a bus where each event type buffers `capacity` events per subscriber.
    ///
    /// A subscriber that falls further behind than `capacity` misses events and is told so
    /// via [`RecvError::Lagged`](super::RecvError::Lagged).
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize, clock: SharedClock) -> Self {
        assert!(capacity > 0, "event bus capacity must be non-zero");
        Self {
            inner: Arc::new(Inner {
                capacity,
                clock,
                channels: RwLock::new(HashMap::new()),
                firehose: broadcast::channel(capacity).0,
            }),
        }
    }

    /// Publishes an event, attributing nothing.
    ///
    /// Prefer [`ComponentContext::publish`](crate::context::ComponentContext::publish),
    /// which records the publishing component.
    ///
    /// Returns how many typed subscribers received it. Publishing to nobody is normal and
    /// is not an error.
    pub fn publish<E: Event>(&self, event: E) -> usize {
        self.publish_envelope(Envelope::stamped(event, self.inner.clock.now()))
    }

    /// Publishes a pre-built envelope, preserving its metadata.
    ///
    /// This is how provenance (source component, correlation id) is carried, and how an
    /// event received from another process can be republished locally without losing its
    /// original identity.
    pub fn publish_envelope<E: Event>(&self, envelope: Envelope<E>) -> usize {
        let delivered = self.sender::<E>().send(envelope.clone()).unwrap_or(0);
        self.fan_out_to_firehose(envelope);
        delivered
    }

    /// Builds metadata for an event of type `E`, stamped with the bus clock.
    pub fn metadata_for<E: Event>(&self) -> EventMetadata {
        EventMetadata::new::<E>(self.inner.clock.now())
    }

    /// Subscribes to one event type.
    ///
    /// Only events published after this call are delivered.
    pub fn subscribe<E: Event>(&self) -> EventStream<E> {
        EventStream::new(E::event_name(), self.sender::<E>().subscribe())
    }

    /// Subscribes to every event, as JSON.
    ///
    /// This is the seam for bridges that must observe the whole system without depending
    /// on its event types: a chat frontend, a shell socket, an audit log. The payload is
    /// the event serialised with `serde_json`; the metadata identifies which event it is.
    pub fn subscribe_any(&self) -> EventStream<Value> {
        EventStream::new(EventName::new("*"), self.inner.firehose.subscribe())
    }

    /// Returns how many subscribers are listening for `E`.
    pub fn subscriber_count<E: Event>(&self) -> usize {
        self.sender::<E>().receiver_count()
    }

    /// Returns how many subscribers are listening to the firehose.
    pub fn firehose_subscriber_count(&self) -> usize {
        self.inner.firehose.receiver_count()
    }

    /// Returns the per-event-type channel capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Serialises and forwards an event to the firehose, but only if anyone is listening.
    ///
    /// Skipping the work when there are no firehose subscribers is what keeps a
    /// serialisable event model free in the common in-process case.
    fn fan_out_to_firehose<E: Event>(&self, envelope: Envelope<E>) {
        if self.inner.firehose.receiver_count() == 0 {
            return;
        }
        match envelope.try_map(|payload| serde_json::to_value(payload)) {
            Ok(json) => {
                let _ = self.inner.firehose.send(json);
            }
            Err(error) => {
                // An event that cannot be serialised is a bug in that event type, but it
                // must not break delivery to typed subscribers, which already succeeded.
                tracing::error!(event = E::NAME, %error, "failed to serialise event for the firehose");
            }
        }
    }

    fn sender<E: Event>(&self) -> broadcast::Sender<Envelope<E>> {
        let key = TypeId::of::<E>();

        if let Some(sender) = self
            .inner
            .channels
            .read()
            .expect("event bus lock poisoned")
            .get(&key)
            .and_then(|erased| erased.downcast_ref::<broadcast::Sender<Envelope<E>>>())
        {
            return sender.clone();
        }

        let mut channels = self.inner.channels.write().expect("event bus lock poisoned");
        let entry = channels.entry(key).or_insert_with(|| {
            Box::new(broadcast::channel::<Envelope<E>>(self.inner.capacity).0)
        });
        entry
            .downcast_ref::<broadcast::Sender<Envelope<E>>>()
            .expect("channel registered under the wrong type")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ManualClock, Timestamp};
    use crate::event::RecvError;
    use crate::id::ComponentId;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Ping {
        seq: u32,
    }

    impl Event for Ping {
        const NAME: &'static str = "test.ping";
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Pong;

    impl Event for Pong {
        const NAME: &'static str = "test.pong";
    }

    #[tokio::test]
    async fn subscribers_receive_only_their_own_event_type() {
        let bus = EventBus::default();
        let mut pings = bus.subscribe::<Ping>();
        let mut pongs = bus.subscribe::<Pong>();

        bus.publish(Ping { seq: 1 });

        assert_eq!(pings.recv().await.unwrap().payload, Ping { seq: 1 });
        assert!(pongs.try_recv().is_none());
    }

    #[tokio::test]
    async fn publishing_to_nobody_is_fine() {
        let bus = EventBus::default();
        assert_eq!(bus.publish(Ping { seq: 1 }), 0);
    }

    #[tokio::test]
    async fn every_subscriber_gets_every_event() {
        let bus = EventBus::default();
        let mut first = bus.subscribe::<Ping>();
        let mut second = bus.subscribe::<Ping>();

        assert_eq!(bus.publish(Ping { seq: 7 }), 2);

        assert_eq!(first.recv().await.unwrap().payload.seq, 7);
        assert_eq!(second.recv().await.unwrap().payload.seq, 7);
    }

    #[tokio::test]
    async fn the_firehose_sees_every_type_as_json() {
        let bus = EventBus::default();
        let mut any = bus.subscribe_any();

        bus.publish(Ping { seq: 3 });
        bus.publish(Pong);

        let first = any.recv().await.unwrap();
        assert_eq!(first.metadata.name.as_str(), "test.ping");
        assert_eq!(first.payload["seq"], serde_json::json!(3));

        let second = any.recv().await.unwrap();
        assert_eq!(second.metadata.name.as_str(), "test.pong");
    }

    #[tokio::test]
    async fn envelope_metadata_is_preserved_across_the_firehose() {
        let bus = EventBus::default();
        let mut any = bus.subscribe_any();

        let metadata = bus
            .metadata_for::<Ping>()
            .with_source(ComponentId::new("demo"));
        let id = metadata.id;
        bus.publish_envelope(Envelope::new(metadata, Ping { seq: 1 }));

        let received = any.recv().await.unwrap();
        assert_eq!(received.metadata.id, id);
        assert_eq!(received.metadata.source, Some(ComponentId::new("demo")));
    }

    #[tokio::test]
    async fn events_are_stamped_with_the_injected_clock() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_234)));
        let bus = EventBus::new(8, clock.clone());
        let mut pings = bus.subscribe::<Ping>();

        bus.publish(Ping { seq: 1 });
        assert_eq!(
            pings.recv().await.unwrap().metadata.timestamp,
            Timestamp::from_millis(1_234)
        );

        clock.advance(std::time::Duration::from_secs(1));
        bus.publish(Ping { seq: 2 });
        assert_eq!(
            pings.recv().await.unwrap().metadata.timestamp,
            Timestamp::from_millis(2_234)
        );
    }

    #[tokio::test]
    async fn slow_subscribers_are_told_what_they_missed() {
        let bus = EventBus::new(2, Arc::new(SystemClock));
        let mut pings = bus.subscribe::<Ping>();

        for seq in 0..5 {
            bus.publish(Ping { seq });
        }

        assert_eq!(pings.recv().await, Err(RecvError::Lagged { count: 3 }));
        // Still usable afterwards: the oldest surviving event is next.
        assert_eq!(pings.recv().await.unwrap().payload.seq, 3);
    }

    #[tokio::test]
    async fn a_late_subscriber_misses_earlier_events() {
        let bus = EventBus::default();
        bus.publish(Ping { seq: 1 });
        let mut pings = bus.subscribe::<Ping>();
        bus.publish(Ping { seq: 2 });

        assert_eq!(pings.recv().await.unwrap().payload.seq, 2);
    }
}
