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
    /// Creates a principal of the given kind.
    pub fn new(id: impl Into<PrincipalId>, kind: PrincipalKind) -> Self {
        Self {
            id: id.into(),
            kind,
            on_behalf_of: None,
        }
    }

    /// Records that this principal is acting for another.
    #[must_use]
    pub fn on_behalf_of(mut self, principal: impl Into<PrincipalId>) -> Self {
        self.on_behalf_of = Some(principal.into());
        self
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
    async fn request_approval(
        &self,
        request: &PermissionRequest,
        prompt: &str,
        cx: &ExecutionContext,
    ) -> Result<bool>;
}
