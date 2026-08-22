//! The ambient context an operation runs in.
//!
//! Anything the system does on someone's behalf — answering a message, running a tool,
//! calling a model, recalling a memory — happens inside an [`ExecutionContext`]. It
//! carries the four things every subsystem needs and none of them should invent
//! separately: who asked, which logical operation this belongs to, when to give up, and
//! how to be cancelled.
//!
//! Propagating one context through a call chain is what makes an operation traceable,
//! cancellable and attributable end to end.

use aik_core::clock::{Clock, Timestamp};
use aik_core::id::CorrelationId;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::permission::Principal;

/// The context one operation runs in.
///
/// Cloning yields a context that shares the same cancellation signal; use
/// [`ExecutionContext::child`] for a sub-operation that can be cancelled independently.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// Ties everything done for one logical operation together.
    pub correlation: CorrelationId,
    /// On whose behalf the operation runs, if anyone's.
    ///
    /// `None` means the system itself — a scheduled job, a startup task. Policy engines
    /// are expected to treat that as a distinct principal, not as "unauthenticated".
    pub principal: Option<Principal>,
    /// When the operation should give up, if ever.
    pub deadline: Option<Timestamp>,
    /// Cancellation for this operation and its children.
    pub cancellation: CancellationToken,
    /// Free-form annotations passed along the call chain, e.g. a session or channel id.
    pub attributes: Map<String, Value>,
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionContext {
    /// Creates a root context for a new logical operation.
    pub fn new() -> Self {
        Self {
            correlation: CorrelationId::new(),
            principal: None,
            deadline: None,
            cancellation: CancellationToken::new(),
            attributes: Map::new(),
        }
    }

    /// Attributes the operation to a principal.
    #[must_use]
    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = Some(principal);
        self
    }

    /// Sets a deadline.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Timestamp) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Adds an annotation.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Derives a sub-operation context.
    ///
    /// It keeps the correlation id, principal, deadline and annotations, but gets its own
    /// cancellation token: cancelling the parent cancels the child, not the other way
    /// round.
    pub fn child(&self) -> Self {
        Self {
            correlation: self.correlation,
            principal: self.principal.clone(),
            deadline: self.deadline,
            cancellation: self.cancellation.child_token(),
            attributes: self.attributes.clone(),
        }
    }

    /// The principal this operation acts as, defaulting to the system.
    ///
    /// A context with no principal is the system acting **on its own behalf** — a startup
    /// task, a scheduled firing, a maintenance sweep. It is a specific identity, not a
    /// wildcard and not "unauthenticated": [`Principal::system`] owns what it creates and
    /// [`Principal::may_act_for`](crate::permission::Principal::may_act_for) gives it no
    /// authority over anybody else's records.
    ///
    /// This lives here, on the context, because it is a security default rather than a
    /// convenience. Every subsystem that owns resources — the context store, the memory
    /// store, the scheduler, the tool registry — has to answer "who is this?" the same way,
    /// and a copy of the answer per crate is a copy that can drift. There is one answer, and
    /// this is it.
    ///
    /// ```
    /// use aik_api::execution::ExecutionContext;
    /// use aik_api::permission::{Principal, PrincipalKind};
    ///
    /// assert_eq!(ExecutionContext::new().principal_or_system(), Principal::system());
    ///
    /// let alice = Principal::new("alice", PrincipalKind::User);
    /// let cx = ExecutionContext::new().with_principal(alice.clone());
    /// assert_eq!(cx.principal_or_system(), alice);
    /// ```
    pub fn principal_or_system(&self) -> Principal {
        self.principal.clone().unwrap_or_else(Principal::system)
    }

    /// Returns true if the operation has been cancelled or its deadline has passed.
    pub fn is_expired(&self, clock: &dyn Clock) -> bool {
        self.cancellation.is_cancelled()
            || self
                .deadline
                .is_some_and(|deadline| clock.now() >= deadline)
    }

    /// Waits until the operation is cancelled.
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
}
