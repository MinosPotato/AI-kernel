//! [`ApprovalBroker`]: the asking half.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ApprovalSink, PermissionRequest};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, Result};
use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};

use crate::gate::{ApprovalGate, ApprovalId, PendingApproval};

/// How long a question waits for an answer when nothing else limits it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How many questions may await an answer at once, by default.
pub const DEFAULT_MAX_PENDING: usize = 16;

/// How many notifications a frontend may fall behind by before it must fall back to
/// [`ApprovalGate::pending`].
pub const NOTIFICATION_CAPACITY: usize = 64;

/// How an [`ApprovalBroker`] behaves.
#[derive(Debug, Clone)]
pub struct ApprovalSettings {
    /// The longest a single question waits for an answer.
    ///
    /// A shorter [`ExecutionContext::deadline`] always wins; this only bounds requests that
    /// would otherwise wait indefinitely. Expiry is a refusal, never an allow.
    pub timeout: Duration,

    /// How many questions may await an answer at once.
    ///
    /// Beyond this, further requests are refused immediately rather than queued. An agent
    /// that emits tool calls faster than a human can read them would otherwise be able to
    /// grow this queue without bound, and a person facing a thousand identical prompts
    /// approves them the way anyone would: carelessly. Refusing early keeps both the memory
    /// and the human's attention bounded.
    pub max_pending: usize,

    /// The clock used for timestamps and deadline arithmetic.
    pub clock: SharedClock,
}

impl Default for ApprovalSettings {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_pending: DEFAULT_MAX_PENDING,
            clock: Arc::new(SystemClock),
        }
    }
}

/// One question parked on the broker, and where to send its answer.
#[derive(Debug)]
pub(crate) struct Waiter {
    pub(crate) pending: PendingApproval,
    pub(crate) answer: oneshot::Sender<bool>,
}

/// State shared by the broker and every [`ApprovalGate`] attached to it.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) waiting: Mutex<HashMap<ApprovalId, Waiter>>,
    pub(crate) notifications: broadcast::Sender<PendingApproval>,
    pub(crate) gates: AtomicUsize,
    pub(crate) closed: AtomicBool,
    pub(crate) settings: ApprovalSettings,
}

impl Shared {
    /// Removes a question, whether it was answered, abandoned or expired.
    pub(crate) fn withdraw(&self, id: &ApprovalId) -> Option<Waiter> {
        match self.waiting.lock() {
            Ok(mut waiting) => waiting.remove(id),
            // Only reachable if a responder panicked mid-answer. Losing the waiter is the
            // fail-closed outcome: the requester times out rather than being granted.
            Err(poisoned) => poisoned.into_inner().remove(id),
        }
    }
}

/// A rendezvous between an authorization check and a human.
///
/// The broker is an [`ApprovalSink`], so it plugs into
/// [`ToolRegistry`](aik_api::tool::ToolRegistry) exactly where the contract expects one; it
/// is *only* an `ApprovalSink`, so holding it grants no ability to approve anything. That
/// belongs to [`ApprovalGate`], obtained from [`ApprovalBroker::gate`].
///
/// # Why a rendezvous rather than a callback
///
/// The obvious alternative — hand the sink a closure that prompts — was rejected because
/// the frontend that must answer is neither in the same crate nor, in general, in the same
/// thread of control: a desktop popup, a chat message and a terminal prompt all resolve
/// asynchronously and can be replaced at runtime. Parking the question in a queue that any
/// frontend can drain keeps the authorization path identical in all of those cases, and
/// means a frontend that dies mid-question causes a timeout rather than a lost answer.
///
/// # Delivery
///
/// A frontend learns about a question two ways, and needs both:
///
/// * [`ApprovalStream`](crate::ApprovalStream), a broadcast of new questions, for waking up
///   promptly. It is lossy under load, by design — a bounded channel that drops the oldest
///   notification is better than one that blocks the authorization path.
/// * [`ApprovalGate::pending`], the authoritative snapshot, for recovering everything a
///   dropped notification would have hidden.
///
/// Losing a notification therefore delays an answer; it never loses a question, and it can
/// never turn one into an approval.
pub struct ApprovalBroker {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ApprovalBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalBroker")
            .field("pending", &self.pending_count())
            .field("gates", &self.gate_count())
            .field("closed", &self.is_closed())
            .field("timeout", &self.shared.settings.timeout)
            .field("max_pending", &self.shared.settings.max_pending)
            .finish()
    }
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalBroker {
    /// Creates a broker with [`ApprovalSettings::default`].
    pub fn new() -> Self {
        Self::with_settings(ApprovalSettings::default())
    }

    /// Creates a broker with explicit settings.
    pub fn with_settings(settings: ApprovalSettings) -> Self {
        Self {
            shared: Arc::new(Shared {
                waiting: Mutex::new(HashMap::new()),
                notifications: broadcast::channel(NOTIFICATION_CAPACITY).0,
                gates: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                settings,
            }),
        }
    }

    /// Attaches a responder and returns the handle it answers through.
    ///
    /// Attaching is what tells the broker somebody is listening: while no gate exists, every
    /// question is refused immediately instead of waiting for a timeout. Dropping the gate
    /// detaches again, so a frontend that exits stops the system from asking questions
    /// nobody will see.
    pub fn gate(&self) -> ApprovalGate {
        ApprovalGate::attach(self.shared.clone())
    }

    /// Refuses every waiting question and every future one.
    ///
    /// Called by [`ApprovalComponent`](crate::ApprovalComponent) on shutdown: a question
    /// whose answer would arrive after the system stopped must not be left waiting, and must
    /// certainly not be granted. Closing is permanent.
    pub fn close(&self) {
        // Set before draining, and under the same lock the requester takes, so a request
        // cannot slip into the queue behind the drain and wait for an answer nobody can
        // give.
        let drained = {
            let mut waiting = match self.shared.waiting.lock() {
                Ok(waiting) => waiting,
                Err(poisoned) => poisoned.into_inner(),
            };
            self.shared.closed.store(true, Ordering::SeqCst);
            std::mem::take(&mut *waiting)
        };
        // Dropping each `Waiter` drops its answer channel, which is what wakes the
        // requesters — with a refusal, since a dropped channel is not an answer.
        drop(drained);
    }

    /// Returns true once [`ApprovalBroker::close`] has been called.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// How many questions are waiting for an answer.
    pub fn pending_count(&self) -> usize {
        match self.shared.waiting.lock() {
            Ok(waiting) => waiting.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// How many [`ApprovalGate`]s are attached.
    pub fn gate_count(&self) -> usize {
        self.shared.gates.load(Ordering::SeqCst)
    }

    /// The settings this broker was built with.
    pub fn settings(&self) -> &ApprovalSettings {
        &self.shared.settings
    }

    /// Parks a question, or explains why it cannot be asked at all.
    fn enqueue(&self, pending: &PendingApproval, answer: oneshot::Sender<bool>) -> Result<()> {
        let mut waiting = match self.shared.waiting.lock() {
            Ok(waiting) => waiting,
            Err(poisoned) => poisoned.into_inner(),
        };

        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        if waiting.len() >= self.shared.settings.max_pending {
            return Err(Error::PermissionDenied(format!(
                "{} approval requests are already awaiting an answer",
                waiting.len()
            )));
        }

        waiting.insert(
            pending.id,
            Waiter {
                pending: pending.clone(),
                answer,
            },
        );
        Ok(())
    }

    /// When this question gives up, being the earlier of the caller's deadline and the
    /// configured timeout.
    fn expiry(&self, now: Timestamp, cx: &ExecutionContext) -> Timestamp {
        let configured = now.saturating_add(self.shared.settings.timeout);
        match cx.deadline {
            Some(deadline) if deadline < configured => deadline,
            _ => configured,
        }
    }
}

/// Removes a question from the queue however the request ends.
///
/// A `Drop` guard rather than cleanup at each exit: the request future can be dropped
/// outright by a caller that stops polling it, and an entry left behind would keep a
/// question visible to a frontend long after anyone was listening for the answer.
struct Withdrawal {
    shared: Arc<Shared>,
    id: ApprovalId,
}

impl Drop for Withdrawal {
    fn drop(&mut self) {
        self.shared.withdraw(&self.id);
    }
}

#[async_trait]
impl ApprovalSink for ApprovalBroker {
    async fn request_approval(
        &self,
        request: &PermissionRequest,
        prompt: &str,
        cx: &ExecutionContext,
    ) -> Result<bool> {
        if self.is_closed() {
            return Err(Error::Cancelled);
        }
        // Asking about an operation that has already given up would put a question in front
        // of a human whose answer could not be used.
        if cx.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.gate_count() == 0 {
            return Err(Error::PermissionDenied(
                "no approval responder is attached, so nobody can answer".into(),
            ));
        }

        let now = self.shared.settings.clock.now();
        let expires_at = self.expiry(now, cx);
        let remaining = expires_at.saturating_since(now);
        if remaining.is_zero() {
            return Err(Error::Timeout(Duration::ZERO));
        }

        let (sender, receiver) = oneshot::channel();
        let pending = PendingApproval {
            id: ApprovalId::new(),
            request: request.clone(),
            prompt: prompt.to_owned(),
            correlation: cx.correlation,
            requested_at: now,
            expires_at,
        };
        self.enqueue(&pending, sender)?;
        let _withdrawal = Withdrawal {
            shared: self.shared.clone(),
            id: pending.id,
        };

        // No subscribers is not a failure: a gate that only polls `pending()` is a valid
        // frontend, and the question is already visible there.
        let _ = self.shared.notifications.send(pending);

        tokio::select! {
            // Cancellation is checked first, so an operation that is both cancelled and
            // overdue reports the more specific reason.
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(remaining) => Err(Error::Timeout(remaining)),
            answer = receiver => match answer {
                Ok(granted) => Ok(granted),
                // The queue dropped the channel without answering: the broker closed.
                Err(_) => Err(Error::PermissionDenied(
                    "the approval mechanism closed before an answer arrived".into(),
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{ActionId, Principal, PrincipalKind, ResourceId};

    fn question() -> PermissionRequest {
        PermissionRequest {
            principal: Principal::new("agent-1", PrincipalKind::Agent),
            action: ActionId::new("filesystem.write"),
            resource: Some(ResourceId::new("/tmp/notes.md")),
            context: serde_json::Value::Null,
        }
    }

    async fn ask(broker: &ApprovalBroker, cx: &ExecutionContext) -> Result<bool> {
        broker.request_approval(&question(), "may I?", cx).await
    }

    #[tokio::test]
    async fn a_broker_with_no_gate_refuses_immediately() {
        let broker = ApprovalBroker::new();
        let error = ask(&broker, &ExecutionContext::new()).await.unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        assert_eq!(broker.pending_count(), 0);
    }

    #[tokio::test]
    async fn dropping_the_last_gate_detaches_again() {
        let broker = ApprovalBroker::new();
        let gate = broker.gate();
        assert_eq!(broker.gate_count(), 1);
        let clone = gate.clone();
        assert_eq!(broker.gate_count(), 2);
        drop(clone);
        assert_eq!(broker.gate_count(), 1);
        drop(gate);
        assert_eq!(broker.gate_count(), 0);

        let error = ask(&broker, &ExecutionContext::new()).await.unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    }

    #[tokio::test]
    async fn an_already_cancelled_context_is_never_asked_about() {
        let broker = ApprovalBroker::new();
        let _gate = broker.gate();
        let cx = ExecutionContext::new();
        cx.cancellation.cancel();

        let error = ask(&broker, &cx).await.unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error}");
        assert_eq!(broker.pending_count(), 0);
    }

    #[tokio::test]
    async fn a_deadline_that_has_already_passed_expires_without_asking() {
        let broker = ApprovalBroker::new();
        let _gate = broker.gate();
        let cx = ExecutionContext::new().with_deadline(Timestamp::EPOCH);

        let error = ask(&broker, &cx).await.unwrap_err();
        assert!(matches!(error, Error::Timeout(_)), "{error}");
        assert_eq!(broker.pending_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_refuses_rather_than_growing() {
        let broker = Arc::new(ApprovalBroker::with_settings(ApprovalSettings {
            max_pending: 2,
            ..Default::default()
        }));
        let _gate = broker.gate();

        for _ in 0..2 {
            let broker = broker.clone();
            tokio::spawn(async move { ask(&broker, &ExecutionContext::new()).await });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(broker.pending_count(), 2);

        let error = ask(&broker, &ExecutionContext::new()).await.unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_question_expires_and_leaves_nothing_behind() {
        let broker = ApprovalBroker::with_settings(ApprovalSettings {
            timeout: Duration::from_secs(5),
            ..Default::default()
        });
        let _gate = broker.gate();

        let error = ask(&broker, &ExecutionContext::new()).await.unwrap_err();

        assert!(matches!(error, Error::Timeout(_)), "{error}");
        assert_eq!(broker.pending_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_context_deadline_wins_when_it_is_shorter() {
        let settings = ApprovalSettings {
            timeout: Duration::from_secs(3_600),
            ..Default::default()
        };
        let deadline = settings.clock.now().saturating_add(Duration::from_secs(1));
        let broker = ApprovalBroker::with_settings(settings);
        let _gate = broker.gate();
        let cx = ExecutionContext::new().with_deadline(deadline);

        let started = tokio::time::Instant::now();
        let error = ask(&broker, &cx).await.unwrap_err();

        assert!(matches!(error, Error::Timeout(_)), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "waited too long"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn closing_refuses_the_waiting_and_the_future() {
        let broker = Arc::new(ApprovalBroker::new());
        let _gate = broker.gate();

        let waiting = tokio::spawn({
            let broker = broker.clone();
            async move { ask(&broker, &ExecutionContext::new()).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(broker.pending_count(), 1);

        broker.close();

        let error = waiting.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        assert!(broker.is_closed());
        assert_eq!(broker.pending_count(), 0);

        let error = ask(&broker, &ExecutionContext::new()).await.unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error}");
    }
}
