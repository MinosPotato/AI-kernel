//! Subscription handles.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures_core::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::Envelope;
use crate::id::EventName;

/// Why a subscription stopped yielding events.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    /// The bus was dropped; no further events will ever arrive.
    #[error("the event bus was closed")]
    Closed,
    /// The subscriber fell behind and `count` events were dropped for it.
    ///
    /// This is recoverable: the next `recv` returns the oldest event still buffered. It
    /// means the subscriber is too slow, or the bus capacity is too small.
    #[error("subscriber lagged and missed {count} events")]
    Lagged {
        /// How many events were dropped.
        count: u64,
    },
}

/// A subscription to one event type, or to the JSON firehose.
///
/// Use [`recv`](EventStream::recv) for a simple loop, or
/// [`into_stream`](EventStream::into_stream) to compose with other streams. Dropping the
/// handle unsubscribes.
#[derive(Debug)]
pub struct EventStream<T> {
    name: EventName,
    receiver: broadcast::Receiver<Envelope<T>>,
}

impl<T: Clone + Send + 'static> EventStream<T> {
    pub(super) fn new(name: EventName, receiver: broadcast::Receiver<Envelope<T>>) -> Self {
        Self { name, receiver }
    }

    /// The event name this subscription is for. The firehose reports `*`.
    pub fn name(&self) -> &EventName {
        &self.name
    }

    /// Waits for the next event.
    ///
    /// Returns [`RecvError::Lagged`] if the subscriber fell behind; the subscription stays
    /// usable afterwards.
    pub async fn recv(&mut self) -> Result<Envelope<T>, RecvError> {
        match self.receiver.recv().await {
            Ok(envelope) => Ok(envelope),
            Err(broadcast::error::RecvError::Closed) => Err(RecvError::Closed),
            Err(broadcast::error::RecvError::Lagged(count)) => Err(RecvError::Lagged { count }),
        }
    }

    /// Takes the next event if one is already buffered.
    pub fn try_recv(&mut self) -> Option<Result<Envelope<T>, RecvError>> {
        match self.receiver.try_recv() {
            Ok(envelope) => Some(Ok(envelope)),
            Err(broadcast::error::TryRecvError::Empty) => None,
            Err(broadcast::error::TryRecvError::Closed) => Some(Err(RecvError::Closed)),
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                Some(Err(RecvError::Lagged { count }))
            }
        }
    }

    /// Creates an independent subscription starting from the current position.
    pub fn resubscribe(&self) -> Self {
        Self {
            name: self.name.clone(),
            receiver: self.receiver.resubscribe(),
        }
    }

    /// Converts this subscription into a [`Stream`].
    ///
    /// Lag is logged and skipped rather than surfaced, because a `Stream` of events has no
    /// natural way to express "you missed some" without forcing every consumer to handle
    /// it. Use [`recv`](EventStream::recv) if lag must be observed.
    pub fn into_stream(self) -> EventStreamAdapter<T> {
        EventStreamAdapter {
            name: self.name,
            inner: BroadcastStream::new(self.receiver),
        }
    }
}

/// A [`Stream`] over an [`EventStream`], skipping lag.
#[derive(Debug)]
pub struct EventStreamAdapter<T> {
    name: EventName,
    inner: BroadcastStream<Envelope<T>>,
}

impl<T: Clone + Send + 'static> Stream for EventStreamAdapter<T> {
    type Item = Envelope<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
                Some(Ok(envelope)) => return Poll::Ready(Some(envelope)),
                Some(Err(BroadcastStreamRecvError::Lagged(count))) => {
                    tracing::warn!(event = %this.name, missed = count, "event subscriber lagged");
                }
                None => return Poll::Ready(None),
            }
        }
    }
}
