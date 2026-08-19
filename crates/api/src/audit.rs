//! Structured authorization and invocation events.
//!
//! Every authorization decision and every tool invocation is published on the kernel
//! [`EventBus`](aik_core::EventBus) as one of the events defined here. This reuses the
//! kernel's existing event mechanism rather than introducing a logging system of its own:
//! an audit sink is just another subscriber, and because every kernel event is
//! serialisable, an out-of-process auditor can consume them through the
//! [firehose](aik_core::EventBus::subscribe_any) without linking against this crate.
//!
//! Nothing here persists anything. These are the contract and the publication mechanism
//! only; durable storage is a subscriber's problem, and deliberately not the kernel's.
//!
//! # What these events must never carry
//!
//! Audit records are the most widely copied, longest-lived data a system produces — they
//! get shipped to log aggregators, held for years, and read by people and tools that were
//! never in the original trust boundary. So they carry the *shape* of what happened, never
//! its contents:
//!
//! * **Tool arguments are never included.** A `filesystem.write` call's arguments contain
//!   the bytes being written; a hypothetical `http.request` call's arguments could contain
//!   an API token. Recording `action`, `resource` and the decision answers every audit
//!   question ("who was allowed to write where, and when") without any of that.
//! * **Tool output is never included**, for the same reason.
//! * [`Decision::Deny`](crate::permission::Decision::Deny) reasons *are* included, since
//!   they are authored by the policy engine — trusted, human-facing text — not by a model,
//!   a tool, or a user.
//!
//! A [`ResourceId`] is the one field that carries caller-influenced content, because an
//! audit trail that omits *what was touched* is not an audit trail. Policy authors should
//! treat resource identifiers as recorded in the clear.
//!
//! # Correlating decisions with execution
//!
//! Every event carries the [`CorrelationId`] of the
//! [`ExecutionContext`](crate::execution::ExecutionContext) it happened under, so the
//! authorization decisions and the invocation they gated join on one key. The kernel also
//! stamps the correlation id and the publishing component onto the event envelope; it is
//! repeated inside the payload so that an audit record remains self-contained if it is
//! ever separated from its envelope.

use aik_core::Event;
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use serde::{Deserialize, Serialize};

use crate::permission::{ActionId, PrincipalId, PrincipalKind, ResourceId};
use crate::tool::ToolName;

/// Which authorization stage a decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationPhase {
    /// "May this principal use this capability at all?" — resolved from
    /// [`ToolSpec::required_permissions`](crate::tool::ToolSpec::required_permissions)
    /// before the tool runs.
    Tool,
    /// "May this principal use it on *this* resource?" — resolved from
    /// [`Tool::planned_resources`](crate::tool::Tool::planned_resources) before the tool
    /// runs.
    Resource,
    /// The same question, asked by the tool mid-execution about a resource it only
    /// discovered while running.
    DiscoveredResource,
}

/// How an authorization question was answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AuthorizationOutcome {
    /// The policy engine allowed it outright.
    Allowed,
    /// The policy engine refused.
    Denied {
        /// The policy engine's stated reason.
        reason: String,
    },
    /// The policy engine deferred to a human, who approved.
    ApprovalGranted,
    /// The policy engine deferred to a human, who refused.
    ApprovalRefused,
    /// The policy engine deferred to a human, but no answer could be obtained.
    ///
    /// Covers both "no approval sink is configured" and a sink that failed to produce an
    /// answer — nobody attached to ask, nobody answered before the deadline, the frontend
    /// went away. Treated as a denial. Recorded distinctly because it means the deployment
    /// is misconfigured or unattended, not that anyone actually said no.
    ApprovalUnavailable,
    /// No policy decision could be obtained, so nothing could be allowed.
    ///
    /// Covers both "no policy engine is configured" and an engine that failed to evaluate.
    /// Also a denial, also a broken mechanism rather than a decision.
    PolicyUnavailable,
}

impl AuthorizationOutcome {
    /// True only when the operation was permitted.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed | Self::ApprovalGranted)
    }
}

/// One authorization question and its answer.
///
/// Published once per question — so a tool requiring two permissions and touching three
/// resources produces five of these before it executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationDecided {
    /// The operation this decision belongs to.
    pub correlation: CorrelationId,
    /// When the decision was made, by the kernel clock.
    pub timestamp: Timestamp,
    /// The tool the decision was made for.
    pub tool: ToolName,
    /// Who was asking.
    pub principal: PrincipalId,
    /// What kind of actor they are.
    pub principal_kind: PrincipalKind,
    /// Who they were acting for, if anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
    /// The capability in question.
    pub action: ActionId,
    /// The specific resource, for resource-level decisions.
    ///
    /// `None` for [`AuthorizationPhase::Tool`], where the question is about the capability
    /// itself rather than any particular target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceId>,
    /// Which stage asked.
    pub phase: AuthorizationPhase,
    /// How long this one decision took to reach, in milliseconds.
    ///
    /// Measured from the moment the question was formed to the moment
    /// [`AuthorizationOutcome`] was decided. Includes both policy evaluation and, when the
    /// policy asked for one, the approval wait — see [`AuthorizationDecided::approval_wait_ms`]
    /// to isolate the latter. A locally measured wall-clock duration (`std::time::Instant`),
    /// not a provider- or policy-engine-reported figure.
    #[serde(default)]
    pub duration_ms: u64,
    /// How long of `duration_ms` was spent specifically waiting on
    /// [`ApprovalSink::request_approval`](crate::permission::ApprovalSink::request_approval),
    /// in milliseconds.
    ///
    /// `Some` for [`AuthorizationOutcome::ApprovalGranted`]/
    /// [`AuthorizationOutcome::ApprovalRefused`], and for [`AuthorizationOutcome::ApprovalUnavailable`]
    /// when it resulted from a sink that was asked and failed. `None` for
    /// [`AuthorizationOutcome::Allowed`]/[`AuthorizationOutcome::Denied`]/
    /// [`AuthorizationOutcome::PolicyUnavailable`], and for the [`AuthorizationOutcome::ApprovalUnavailable`]
    /// case where no sink was configured to ask in the first place — in both, no approval
    /// wait happened at all.
    ///
    /// Broken out from `duration_ms` because the two have wildly different distributions: a
    /// policy check is sub-millisecond and in-memory, an approval wait is a human being asked
    /// a question and can run to minutes. Folding them into one number makes that number
    /// bimodal and useless for alerting on either half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_wait_ms: Option<u64>,
    /// The answer.
    #[serde(flatten)]
    pub outcome: AuthorizationOutcome,
}

impl Event for AuthorizationDecided {
    const NAME: &'static str = "aik.authorization.decided";
}

/// How an invocation ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum InvocationOutcome {
    /// The tool ran and reported success.
    Succeeded,
    /// The tool ran and reported a failure the caller was meant to see.
    ///
    /// This is [`ToolOutcome::is_error`](crate::tool::ToolOutcome::is_error) — the tool
    /// worked, the operation did not.
    ReportedError,
    /// The tool could not be run at all.
    Failed {
        /// The kernel error classification, e.g. `cancelled` or `timeout`.
        ///
        /// The classification rather than the message: an error string can embed
        /// caller-supplied content, a `kind` cannot.
        kind: String,
    },
    /// Authorization refused the call, so the tool never ran.
    Denied,
    /// No tool of that name is registered, so nothing ran.
    NotFound,
}

impl InvocationOutcome {
    /// True only when the tool ran and reported success.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// One completed (or refused) tool invocation.
///
/// Published exactly once per [`ToolRegistry::invoke`](crate::tool::ToolRegistry::invoke)
/// call, whatever the result — including calls that were denied or named a tool that does
/// not exist, both of which are worth seeing in an audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvoked {
    /// The operation this invocation belongs to.
    pub correlation: CorrelationId,
    /// When the invocation finished, by the kernel clock.
    pub timestamp: Timestamp,
    /// The tool that was asked for.
    ///
    /// Present even for [`InvocationOutcome::NotFound`], since a caller probing for tools
    /// that do not exist is itself worth recording.
    pub tool: ToolName,
    /// Who was asking.
    pub principal: PrincipalId,
    /// What kind of actor they are.
    pub principal_kind: PrincipalKind,
    /// Who they were acting for, if anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
    /// How long the whole call took, in milliseconds: authorization plus execution.
    ///
    /// Present even for [`InvocationOutcome::NotFound`] and
    /// [`InvocationOutcome::Denied`], where it measures whatever work happened before the
    /// call was refused. A locally measured wall-clock duration, not a provider figure.
    #[serde(default)]
    pub duration_ms: u64,
    /// How long authorization (every [`AuthorizationPhase::Tool`] and
    /// [`AuthorizationPhase::Resource`] question this call required) took, in milliseconds.
    ///
    /// `None` for [`InvocationOutcome::NotFound`], where no authorization question was ever
    /// asked. Includes any approval wait, for the same reason
    /// [`AuthorizationDecided::duration_ms`] does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_duration_ms: Option<u64>,
    /// How long the tool itself ran, in milliseconds, excluding authorization.
    ///
    /// `None` when the tool never ran at all — [`InvocationOutcome::NotFound`] or
    /// [`InvocationOutcome::Denied`]. This is the figure to read as "tool latency" in the
    /// narrow sense; [`ToolInvoked::duration_ms`] is the end-to-end figure most callers
    /// actually experience.
    ///
    /// Note this can still include authorization time: a tool such as
    /// `filesystem.list` asks [`AuthorizationPhase::DiscoveredResource`] questions *while
    /// it runs*, one per entry, which are structurally part of its execution rather than
    /// of the up-front [`ToolInvoked::authorization_duration_ms`] phase. Summing every
    /// [`AuthorizationDecided::duration_ms`] for the same correlation gives the true total
    /// authorization time when that distinction matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_duration_ms: Option<u64>,
    /// How it ended.
    #[serde(flatten)]
    pub outcome: InvocationOutcome,
}

impl Event for ToolInvoked {
    const NAME: &'static str = "aik.tool.invoked";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decision() -> AuthorizationDecided {
        AuthorizationDecided {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1_000),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            action: ActionId::new("demo.act"),
            resource: Some(ResourceId::new("/tmp/x")),
            phase: AuthorizationPhase::Resource,
            duration_ms: 5,
            approval_wait_ms: None,
            outcome: AuthorizationOutcome::Allowed,
        }
    }

    #[test]
    fn authorization_events_round_trip() {
        let event = decision();
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["outcome"], json!("allowed"));
        assert_eq!(json["phase"], json!("resource"));
        assert_eq!(json["resource"], json!("/tmp/x"));
        assert!(json.get("approval_wait_ms").is_none());

        let parsed: AuthorizationDecided = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn an_approval_decision_reports_the_wait_separately_from_the_total() {
        let event = AuthorizationDecided {
            duration_ms: 4_012,
            approval_wait_ms: Some(4_009),
            outcome: AuthorizationOutcome::ApprovalGranted,
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["duration_ms"], json!(4_012));
        assert_eq!(json["approval_wait_ms"], json!(4_009));

        let parsed: AuthorizationDecided = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn denials_carry_the_policy_authored_reason() {
        let event = AuthorizationDecided {
            outcome: AuthorizationOutcome::Denied {
                reason: "outside the workspace".into(),
            },
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["outcome"], json!("denied"));
        assert_eq!(json["reason"], json!("outside the workspace"));
    }

    #[test]
    fn tool_level_decisions_omit_the_resource() {
        let event = AuthorizationDecided {
            resource: None,
            phase: AuthorizationPhase::Tool,
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("resource").is_none());
    }

    #[test]
    fn invocation_events_round_trip() {
        let event = ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(2_000),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: Some(PrincipalId::new("user-1")),
            duration_ms: 12,
            authorization_duration_ms: Some(3),
            execution_duration_ms: Some(9),
            outcome: InvocationOutcome::Failed {
                kind: "timeout".into(),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["result"], json!("failed"));
        assert_eq!(json["kind"], json!("timeout"));
        assert_eq!(json["on_behalf_of"], json!("user-1"));
        assert_eq!(json["duration_ms"], json!(12));
        assert_eq!(json["authorization_duration_ms"], json!(3));
        assert_eq!(json["execution_duration_ms"], json!(9));

        let parsed: ToolInvoked = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn a_tool_invocation_that_never_ran_omits_its_execution_duration() {
        let event = ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(2_000),
            tool: ToolName::new("ghost"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            duration_ms: 1,
            authorization_duration_ms: None,
            execution_duration_ms: None,
            outcome: InvocationOutcome::NotFound,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("authorization_duration_ms").is_none());
        assert!(json.get("execution_duration_ms").is_none());
    }

    #[test]
    fn outcomes_classify_themselves() {
        assert!(AuthorizationOutcome::Allowed.is_allowed());
        assert!(AuthorizationOutcome::ApprovalGranted.is_allowed());
        assert!(!AuthorizationOutcome::ApprovalRefused.is_allowed());
        assert!(!AuthorizationOutcome::ApprovalUnavailable.is_allowed());
        assert!(!AuthorizationOutcome::PolicyUnavailable.is_allowed());
        assert!(!AuthorizationOutcome::Denied { reason: "n".into() }.is_allowed());

        assert!(InvocationOutcome::Succeeded.is_success());
        assert!(!InvocationOutcome::ReportedError.is_success());
        assert!(!InvocationOutcome::Denied.is_success());
    }
}
