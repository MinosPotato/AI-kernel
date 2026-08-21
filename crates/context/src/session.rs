//! The rules every [`ContextStore`](aik_api::context::ContextStore) in this crate applies
//! identically, whatever it stores records in.
//!
//! Both implementations answer the same two non-storage questions on every call — *may this
//! principal touch this session*, and *how is the resulting window reported* — and both
//! answers are security-relevant: the first is the boundary that keeps one user's
//! transcript out of another's prompt, and the second is the reason a
//! [`ContextAssembled`] event carries counts and never content. Two copies of either rule
//! would be two things to keep in step, and a divergence would be silent. So there is one
//! copy, here.

use aik_api::agent::SessionId;
use aik_api::context::ContextAssembled;
use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId};
use aik_core::clock::Timestamp;
use aik_core::event::{Envelope, EventBus};
use aik_core::id::ComponentId;
use aik_core::{Error, Result};

/// The principal a context is acting as.
///
/// A context with no principal is the system acting for itself — its own identity, not a
/// wildcard — exactly as it is in [`ToolRegistry`](aik_api::tool::ToolRegistry).
pub(crate) fn principal_of(cx: &ExecutionContext) -> Principal {
    cx.principal.clone().unwrap_or_else(Principal::system)
}

/// Fails closed unless `principal` owns `session`.
///
/// The principal is passed in rather than the [`ExecutionContext`] because a persistent
/// store resolves it on the async side and checks it on a blocking thread, where the
/// context is no longer in hand.
pub(crate) fn authorize(
    session: &SessionId,
    owner: &PrincipalId,
    principal: &Principal,
) -> Result<()> {
    if principal.may_act_for(owner) {
        return Ok(());
    }
    Err(Error::PermissionDenied(format!(
        "context session `{session}` belongs to `{owner}`, not to `{}`",
        principal.id
    )))
}

/// Publishes [`ContextAssembled`] for whichever store assembled the window.
///
/// Optional throughout: a store built without a bus assembles windows identically and
/// simply is not observable.
pub(crate) struct AssemblyReporter {
    events: Option<EventBus>,
    source: ComponentId,
}

impl std::fmt::Debug for AssemblyReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssemblyReporter")
            .field("configured", &self.events.is_some())
            .field("source", &self.source)
            .finish()
    }
}

impl AssemblyReporter {
    /// A reporter that publishes nothing, attributed to `source` should a bus arrive later.
    pub(crate) fn silent(source: ComponentId) -> Self {
        Self {
            events: None,
            source,
        }
    }

    /// Publishes to `events`, attributed to `source`.
    pub(crate) fn new(events: EventBus, source: ComponentId) -> Self {
        Self {
            events: Some(events),
            source,
        }
    }

    /// Whether anything is listening at all. Used only by `Debug` implementations.
    pub(crate) fn is_configured(&self) -> bool {
        self.events.is_some()
    }

    /// Reports one assembled window.
    ///
    /// `usage` is counts only, by construction: [`ContextAssembled`] has nowhere to put a
    /// message even if a caller wanted to.
    pub(crate) fn report(
        &self,
        cx: &ExecutionContext,
        session: SessionId,
        timestamp: Timestamp,
        usage: aik_api::context::ContextUsage,
    ) {
        let Some(bus) = &self.events else {
            return;
        };
        let metadata = bus
            .metadata_for::<ContextAssembled>()
            .with_source(self.source.clone())
            .with_correlation(cx.correlation);
        bus.publish_envelope(Envelope::new(
            metadata,
            ContextAssembled {
                correlation: cx.correlation,
                timestamp,
                session,
                usage,
            },
        ));
    }
}
