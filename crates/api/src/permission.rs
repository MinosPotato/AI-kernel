//! Authorisation contracts.
//!
//! An AI layer with access to a desktop, a filesystem and a network needs one place where
//! "may this happen?" is answered. The kernel provides no policy; it provides the shape of
//! the question ([`PermissionRequest`]) and the answer ([`Decision`]).
//!
//! Two traits, deliberately separate:
//!
//! * [`PolicyEngine`] decides from rules, without user interaction.
//! * [`ApprovalSink`] asks a human, and is implemented by frontends.
//!
//! Keeping them apart means the policy layer never depends on a UI, and a headless
//! deployment simply has no approval sink — [`Decision::RequireApproval`] then becomes a
//! denial rather than a hang.
//!
//! Neither trait enforces anything by itself; enforcement is a property of whoever calls
//! them. For tools specifically, that caller is
//! [`ToolRegistry`](crate::tool::ToolRegistry) — see its documentation for why the
//! enforcement point matters as much as the policy itself.

use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;

aik_core::string_id! {
    /// Names an actor: a user, an agent, a plugin, a remote caller.
    pub PrincipalId
}

aik_core::string_id! {
    /// Names something that can be done, e.g. `fs.write` or `hyprland.dispatch`.
    pub ActionId
}

aik_core::string_id! {
    /// Names what an action is done to, e.g. a path, a window, a conversation.
    pub ResourceId
}

/// What kind of actor a principal is.
///
/// Policy almost always cares about this distinction — an autonomous agent asking to
/// delete files is not the same as the user asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A human.
    User,
    /// An agent acting autonomously.
    Agent,
    /// A plugin or extension.
    Plugin,
    /// The system itself: scheduled jobs, startup work.
    System,
    /// Something else, e.g. a remote API caller.
    External,
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// The actor's identifier.
    pub id: PrincipalId,
    /// What kind of actor it is.
    pub kind: PrincipalKind,
    /// The principal this one is acting for, if any.
    ///
    /// An agent launched by a user should carry the user here, so policy can distinguish
    /// delegated authority from autonomous action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
}

impl Principal {
    /// The identifier of the implicit system principal.
    pub const SYSTEM: &'static str = "system";

    /// Creates a principal of the given kind.
    pub fn new(id: impl Into<PrincipalId>, kind: PrincipalKind) -> Self {
        Self {
            id: id.into(),
            kind,
            on_behalf_of: None,
        }
    }

    /// The system acting on its own behalf.
    ///
    /// This is what an [`ExecutionContext`] with no principal means — a scheduled job,
    /// startup work — and it is a distinct identity, not a wildcard and not
    /// "unauthenticated". Subsystems that need to attribute such a context should use this
    /// rather than inventing their own name for it, so a policy rule written against
    /// `system` matches everywhere.
    pub fn system() -> Self {
        Self::new(Self::SYSTEM, PrincipalKind::System)
    }

    /// Records that this principal is acting for another.
    #[must_use]
    pub fn on_behalf_of(mut self, principal: impl Into<PrincipalId>) -> Self {
        self.on_behalf_of = Some(principal.into());
        self
    }

    /// Whether this principal may act on resources owned by `owner`.
    ///
    /// True when it *is* the owner, or when it is explicitly acting
    /// [`on_behalf_of`](Principal::on_behalf_of) the owner. Nothing else: the system
    /// principal is an identity like any other here, not a wildcard, and a principal that
    /// merely shares a `kind` with the owner gets nothing.
    ///
    /// # Why this lives on `Principal`
    ///
    /// Every subsystem that owns resources per principal — a context session, a memory
    /// record, and whatever the scheduler ends up storing — has to answer exactly this
    /// question, and it is security-relevant enough that two implementations of it would be
    /// two things to keep in step with a divergence nobody would notice until it let one
    /// user read another's data. So there is one, here, next to the delegation it reads.
    ///
    /// Note the limit, the same one stated for
    /// [context sessions](crate::context#what-the-model-can-and-cannot-touch): in-process
    /// code can construct a `Principal` naming anyone, so this is a boundary against a model
    /// — which can never construct one — and defence in depth against a confused caller. It
    /// is not a boundary against hostile code already inside the process.
    ///
    /// ```
    /// use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
    ///
    /// let alice = Principal::new("alice", PrincipalKind::User);
    /// let her_agent = Principal::new("agent", PrincipalKind::Agent).on_behalf_of("alice");
    /// let mallory = Principal::new("mallory", PrincipalKind::User);
    ///
    /// assert!(alice.may_act_for(&PrincipalId::new("alice")));
    /// assert!(her_agent.may_act_for(&PrincipalId::new("alice")));
    ///
    /// // Delegation runs one way only: acting for Alice does not make Alice's resources
    /// // the agent's, nor the agent's resources Alice's.
    /// assert!(!alice.may_act_for(&PrincipalId::new("agent")));
    /// assert!(!mallory.may_act_for(&PrincipalId::new("alice")));
    ///
    /// // The system is an identity, not a master key.
    /// assert!(!Principal::system().may_act_for(&PrincipalId::new("alice")));
    /// ```
    pub fn may_act_for(&self, owner: &PrincipalId) -> bool {
        &self.id == owner || self.on_behalf_of.as_ref() == Some(owner)
    }
}

/// A question for the policy layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Who is asking.
    pub principal: Principal,
    /// What they want to do.
    pub action: ActionId,
    /// What they want to do it to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceId>,
    /// Anything else the policy may want to consider.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context: Value,
}

/// The answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Go ahead.
    Allow,
    /// Refuse.
    Deny {
        /// Why, for the caller and the audit log.
        reason: String,
    },
    /// Ask a human first.
    RequireApproval {
        /// What to show them.
        prompt: String,
    },
}

impl Decision {
    /// Refuses with a reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    /// Defers to a human.
    pub fn ask(prompt: impl Into<String>) -> Self {
        Self::RequireApproval {
            prompt: prompt.into(),
        }
    }

    /// True only for [`Decision::Allow`].
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Decides what is permitted, from rules alone.
#[async_trait]
pub trait PolicyEngine: Send + Sync + 'static {
    /// Answers a permission question.
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        cx: &ExecutionContext,
    ) -> Result<Decision>;
}

/// Asks a human for approval. Implemented by frontends.
#[async_trait]
pub trait ApprovalSink: Send + Sync + 'static {
    /// Presents a request and waits for an answer.
    ///
    /// Implementations must respect `cx`'s cancellation and deadline: an approval prompt
    /// nobody answers must not block the system forever.
    ///
    /// # Obligations
    ///
    /// * `Ok(true)` means a human actually approved *this* request. Nothing else may
    ///   produce it — not a default, not a retry, not an unreachable frontend.
    /// * `Ok(false)` means a human actually refused.
    /// * Everything else — nobody to ask, nobody answered in time, the frontend failed — is
    ///   `Err`. Callers treat that as a denial either way, but the distinction is what lets
    ///   an audit trail tell a refusal apart from a broken mechanism: see
    ///   [`AuthorizationOutcome::ApprovalUnavailable`](crate::audit::AuthorizationOutcome::ApprovalUnavailable).
    async fn request_approval(
        &self,
        request: &PermissionRequest,
        prompt: &str,
        cx: &ExecutionContext,
    ) -> Result<bool>;
}

/// Authorizes individual resources within one already-scoped operation.
///
/// This is the *asking* half of resource-level authorization, handed to a
/// [`Tool`](crate::tool::Tool) for the duration of a single invocation. It deliberately
/// takes no principal, no correlation id and no [`ExecutionContext`]: all of those are
/// fixed when the authorizer is created, by the trusted code that created it. A tool
/// cannot widen its own scope, ask on someone else's behalf, or outlive the call it was
/// given for.
///
/// Handing this to a tool does **not** move policy into the tool. The tool asks; a
/// [`PolicyEngine`] behind this handle decides; the tool only ever learns yes or no. The
/// distinction that matters is between *asking* and *deciding*, and only the former is
/// delegated.
///
/// # When a tool needs this
///
/// Most tools do not. Resources that are knowable from the arguments should be declared
/// up front via [`Tool::planned_resources`](crate::tool::Tool::planned_resources), which
/// the registry authorizes *before* the tool runs at all — a stronger position, because
/// nothing has executed yet.
///
/// This handle exists for resources that only become known during execution:
///
/// * a path that turned out to be a symlink to somewhere else once resolved;
/// * entries discovered while walking a directory or expanding a glob;
/// * a redirect target a network request was pointed at.
///
/// In each case the resource the tool is *actually* about to touch differs from the one it
/// declared, and re-asking is the only correct response. See the
/// [TOCTOU discussion](crate::tool#time-of-check-to-time-of-use) for why this is a
/// security requirement rather than a convenience.
#[async_trait]
pub trait ResourceAuthorizer: Send + Sync {
    /// Authorizes one action against one resource.
    ///
    /// Returns `Ok(())` only if the operation is permitted. Anything else — a policy
    /// denial, an approval that was refused, an approval that could not be obtained — is
    /// [`Error::PermissionDenied`](aik_core::Error::PermissionDenied). There is
    /// deliberately no boolean form: a `Result` that must be propagated is harder to
    /// accidentally ignore than a `bool` that can be dropped.
    async fn authorize(&self, action: &ActionId, resource: &ResourceId) -> Result<()>;
}
