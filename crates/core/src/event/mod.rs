//! Typed, event-driven communication.
//!
//! Every event type carries a stable wire [`NAME`](Event::NAME) and is serialisable. That
//! combination is what lets the same bus serve two very different consumers:
//!
//! * **In-process subscribers** get statically typed events with no serialisation cost —
//!   each event type has its own broadcast channel, so a subscriber only pays for the
//!   types it listens to.
//! * **Bridges** (a chat frontend, a socket for the shell, a log sink) subscribe to the
//!   [firehose](EventBus::subscribe_any) and receive every event as JSON plus metadata,
//!   without knowing a single event type. Payloads are only serialised when at least one
//!   firehose subscriber exists.
//!
//! Events are notifications, not requests: they are broadcast, delivery is best-effort,
//! and publishing never blocks or fails because nobody is listening. Request/response
//! interactions belong in the [registry](crate::registry) as ordinary trait calls.
//!
//! ```
//! # use aik_core::event::{Event, EventBus};
//! # use serde::{Deserialize, Serialize};
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! struct WorkspaceChanged {
//!     index: u32,
//! }
//!
//! impl Event for WorkspaceChanged {
//!     const NAME: &'static str = "platform.workspace_changed";
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let bus = EventBus::default();
//! let mut events = bus.subscribe::<WorkspaceChanged>();
//! bus.publish(WorkspaceChanged { index: 3 });
//!
//! let received = events.recv().await.unwrap();
//! assert_eq!(received.payload.index, 3);
//! assert_eq!(received.metadata.name.as_str(), "platform.workspace_changed");
//! # }
//! ```

mod bus;
mod stream;

pub use bus::{DEFAULT_EVENT_CAPACITY, EventBus};
pub use stream::{EventStream, EventStreamAdapter, RecvError};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::id::{ComponentId, CorrelationId, EventId, EventName};

/// A type that can be published on the [`EventBus`].
///
/// The `NAME` is a stable wire identifier and should be namespaced and dotted, e.g.
/// `kernel.state_changed`. It is what firehose consumers and out-of-process bridges match
/// on, so changing it is a breaking change in the same way that renaming a public method
/// is.
///
/// The `Serialize`/`DeserializeOwned` bounds are what make cross-process delivery possible
/// later without revisiting every event type.
pub trait Event:
    Clone + Send + Sync + std::fmt::Debug + Serialize + DeserializeOwned + 'static
{
    /// The stable wire name of this event type.
    const NAME: &'static str;

    /// Returns the wire name as an [`EventName`].
    fn event_name() -> EventName {
        EventName::new(Self::NAME)
    }
}

/// Everything the kernel knows about a published event except its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMetadata {
    /// Unique, time-ordered identifier of this particular publication.
    pub id: EventId,
    /// The wire name of the event type.
    pub name: EventName,
    /// When it was published, according to the kernel clock.
    pub timestamp: Timestamp,
    /// The component that published it, if it was published through a component context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ComponentId>,
    /// The logical operation this event belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<CorrelationId>,
}

impl EventMetadata {
    /// Creates metadata for an event of type `E` stamped at `timestamp`.
    pub fn new<E: Event>(timestamp: Timestamp) -> Self {
        Self {
            id: EventId::new(),
            name: E::event_name(),
            timestamp,
            source: None,
            correlation: None,
        }
    }

    /// Attributes the event to a component.
    #[must_use]
    pub fn with_source(mut self, source: ComponentId) -> Self {
        self.source = Some(source);
        self
    }

    /// Ties the event to a logical operation.
    #[must_use]
    pub fn with_correlation(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }
}

/// A payload together with its [`EventMetadata`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Identity, timing and provenance of the event.
    #[serde(flatten)]
    pub metadata: EventMetadata,
    /// The event itself.
    pub payload: T,
}

impl<T> Envelope<T> {
    /// Pairs a payload with metadata.
    pub fn new(metadata: EventMetadata, payload: T) -> Self {
        Self { metadata, payload }
    }

    /// Transforms the payload, keeping the metadata intact.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Envelope<U> {
        Envelope {
            metadata: self.metadata,
            payload: f(self.payload),
        }
    }

    /// Transforms the payload fallibly, keeping the metadata intact.
    pub fn try_map<U, E>(self, f: impl FnOnce(T) -> Result<U, E>) -> Result<Envelope<U>, E> {
        Ok(Envelope {
            metadata: self.metadata,
            payload: f(self.payload)?,
        })
    }
}

impl<E: Event> Envelope<E> {
    /// Creates an envelope for `payload`, stamped at `timestamp`.
    pub fn stamped(payload: E, timestamp: Timestamp) -> Self {
        Self::new(EventMetadata::new::<E>(timestamp), payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Ping {
        seq: u32,
    }

    impl Event for Ping {
        const NAME: &'static str = "test.ping";
    }

    #[test]
    fn envelopes_serialise_metadata_alongside_the_payload() {
        let envelope = Envelope::stamped(Ping { seq: 1 }, Timestamp::from_millis(7));
        let json = serde_json::to_value(&envelope).unwrap();

        assert_eq!(json["name"], json!("test.ping"));
        assert_eq!(json["timestamp"], json!(7));
        assert_eq!(json["payload"]["seq"], json!(1));
        // Absent provenance is omitted rather than serialised as null.
        assert!(json.get("source").is_none());

        let round_tripped: Envelope<Ping> = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.payload, envelope.payload);
        assert_eq!(round_tripped.metadata, envelope.metadata);
    }

    #[test]
    fn mapping_preserves_metadata() {
        let envelope = Envelope::stamped(Ping { seq: 2 }, Timestamp::from_millis(1));
        let id = envelope.metadata.id;
        let mapped = envelope.map(|ping| ping.seq);
        assert_eq!(mapped.payload, 2);
        assert_eq!(mapped.metadata.id, id);
    }
}
