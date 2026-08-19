//! [`InProcessToolRegistry`]: the reference [`ToolRegistry`] implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aik_api::audit::{
    AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase, InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, ApprovalSink, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalId,
    ResourceAuthorizer, ResourceId,
};
use aik_api::tool::{Tool, ToolName, ToolOutcome, ToolRegistry, ToolSpec};
use aik_core::clock::{SharedClock, SystemClock};
use aik_core::event::{Envelope, Event, EventBus};
use aik_core::id::ComponentId;
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde_json::json;

/// The principal attributed to a call whose [`ExecutionContext`] names none.
///
/// A context with no principal is the system acting on its own behalf — a scheduled job,
/// startup work — not an unauthenticated caller. Policy engines should treat it as its own
/// identity rather than as a wildcard.
const SYSTEM_PRINCIPAL: &str = Principal::SYSTEM;

/// A [`ToolRegistry`] that holds its tools in memory and runs them in the same process.
///
/// Registration happens once, at construction — through [`InProcessToolRegistry::register`]
/// — before the registry is ever shared. There is deliberately no way to register a tool
/// through `&self`: by the time anything holds an `Arc<dyn ToolRegistry>`, the set of tools
/// behind it is frozen, so there is no window in which a tool could be added by something
/// other than whatever assembled the registry in the first place.
///
/// Policy and approval are resolved once too, also at construction
/// ([`InProcessToolRegistry::with_policy`], [`InProcessToolRegistry::with_approvals`]) —
/// not by looking a `PolicyEngine` up in the kernel registry at invocation time. That
/// keeps wiring explicit and immune to component start-up ordering: which component
/// happens to register a `dyn PolicyEngine` first, relative to whoever builds this
/// registry, would otherwise silently decide whether tools are enforced at all.
///
/// # Authorization
///
/// [`ToolRegistry::invoke`] resolves, in order, before the tool runs at all:
///
/// 1. each of the tool's [`ToolSpec::required_permissions`], as a capability-level
///    question with no resource;
/// 2. each [`ResourceClaim`](aik_api::tool::ResourceClaim) from
///    [`Tool::planned_resources`].
///
/// The tool then receives a [`ResourceAuthorizer`] for anything it discovers while
/// running. All three phases go through the same policy engine and produce the same audit
/// events; see the [`aik_api::tool`] module documentation for why the split exists.
///
/// Denial is fail-closed at every point where it would be easy to get backwards: a
/// required permission with no policy engine configured is a denial, and a
/// [`Decision::RequireApproval`] with no approval sink configured is a denial rather than a
/// hang or a silent allow. A configured policy engine or approval sink that *fails* is the
/// same: its error stops the call and is recorded as
/// [`AuthorizationOutcome::PolicyUnavailable`] or
/// [`AuthorizationOutcome::ApprovalUnavailable`], never allowed through and never left out
/// of the audit trail.
pub struct InProcessToolRegistry {
    tools: HashMap<ToolName, Arc<dyn Tool>>,
    policy: Option<Arc<dyn PolicyEngine>>,
    approvals: Option<Arc<dyn ApprovalSink>>,
    audit: Option<EventBus>,
    source: ComponentId,
    clock: SharedClock,
}

impl std::fmt::Debug for InProcessToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.tools.keys().map(ToolName::as_str).collect();
        names.sort_unstable();
        f.debug_struct("InProcessToolRegistry")
            .field("tools", &names)
            .field("policy_configured", &self.policy.is_some())
            .field("approvals_configured", &self.approvals.is_some())
            .field("audit_configured", &self.audit.is_some())
            .finish()
    }
}

impl Default for InProcessToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessToolRegistry {
    /// Creates an empty registry with no policy, no approval sink and no audit bus.
    ///
    /// With nothing configured, any tool that declares at least one required permission is
    /// denied unconditionally. Tools that declare no permissions at all still run, since
    /// there is nothing to authorize.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            policy: None,
            approvals: None,
            audit: None,
            source: ComponentId::new("tools.registry"),
            clock: Arc::new(SystemClock),
        }
    }

    /// Configures the policy engine consulted for every permission a tool requires.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn PolicyEngine>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Configures where a [`Decision::RequireApproval`] is resolved.
    ///
    /// Without one, a permission that requires approval is denied rather than left
    /// pending.
    #[must_use]
    pub fn with_approvals(mut self, approvals: Arc<dyn ApprovalSink>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// Publishes audit events to the kernel event bus, attributed to `source`.
    ///
    /// Without a bus, decisions are still enforced exactly the same way — they are simply
    /// not observable. Auditing is not part of the fail-closed guarantee: a missing audit
    /// sink must not stop the system from refusing things correctly.
    #[must_use]
    pub fn with_audit(mut self, events: EventBus, source: ComponentId) -> Self {
        self.audit = Some(events);
        self.source = source;
        self
    }

    /// Overrides the clock used to stamp audit events. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Registers a tool.
    ///
    /// Fails with [`Error::AlreadyExists`] if a tool with the same [`ToolSpec::name`] is
    /// already registered.
    pub fn register(&mut self, tool: impl Tool) -> Result<()> {
        self.register_arc(Arc::new(tool))
    }

    /// Registers an already-shared tool.
    ///
    /// Fails with [`Error::AlreadyExists`] if a tool with the same [`ToolSpec::name`] is
    /// already registered.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.spec().name;
        if self.tools.contains_key(&name) {
            return Err(Error::already_exists("tool", &name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    fn principal_of(cx: &ExecutionContext) -> Principal {
        cx.principal.clone().unwrap_or_else(Principal::system)
    }

    /// Publishes an audit event, correlated and attributed, if a bus is configured.
    fn audit<E: Event>(&self, cx: &ExecutionContext, event: E) {
        let Some(bus) = &self.audit else {
            return;
        };
        let metadata = bus
            .metadata_for::<E>()
            .with_source(self.source.clone())
            .with_correlation(cx.correlation);
        bus.publish_envelope(Envelope::new(metadata, event));
    }

    fn record_decision(
        &self,
        question: &Question<'_>,
        outcome: AuthorizationOutcome,
        duration: Duration,
        approval_wait: Option<Duration>,
    ) {
        self.audit(
            question.cx,
            AuthorizationDecided {
                correlation: question.cx.correlation,
                timestamp: self.clock.now(),
                tool: question.tool.clone(),
                principal: question.principal.id.clone(),
                principal_kind: question.principal.kind,
                on_behalf_of: question.principal.on_behalf_of.clone(),
                action: question.action.clone(),
                resource: question.resource.cloned(),
                phase: question.phase,
                duration_ms: millis(duration),
                approval_wait_ms: approval_wait.map(millis),
                outcome,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_invocation(
        &self,
        cx: &ExecutionContext,
        tool: &ToolName,
        principal: &Principal,
        outcome: InvocationOutcome,
        duration: Duration,
        authorization_duration: Option<Duration>,
        execution_duration: Option<Duration>,
    ) {
        self.audit(
            cx,
            ToolInvoked {
                correlation: cx.correlation,
                timestamp: self.clock.now(),
                tool: tool.clone(),
                principal: principal.id.clone(),
                principal_kind: principal.kind,
                on_behalf_of: principal.on_behalf_of.clone(),
                duration_ms: millis(duration),
                authorization_duration_ms: authorization_duration.map(millis),
                execution_duration_ms: execution_duration.map(millis),
                outcome,
            },
        );
    }

    /// Resolves one authorization question, recording whatever it decides.
    ///
    /// This is the single implementation of "may this happen?" — capability-level checks,
    /// planned-resource checks and resources discovered at runtime all land here, so all
    /// three are enforced identically and audited identically.
    async fn decide(&self, question: &Question<'_>) -> Result<()> {
        let started = Instant::now();
        let subject = question.describe();
        let tool = question.tool;

        let Some(policy) = &self.policy else {
            self.record_decision(
                question,
                AuthorizationOutcome::PolicyUnavailable,
                started.elapsed(),
                None,
            );
            return Err(Error::PermissionDenied(format!(
                "tool `{tool}`: {subject} is required, but no policy engine is configured"
            )));
        };

        let request = PermissionRequest {
            principal: question.principal.clone(),
            action: question.action.clone(),
            resource: question.resource.cloned(),
            context: json!({ "tool": tool.as_str() }),
        };

        // A policy engine that fails to answer has not allowed anything. The failure is
        // recorded before it is propagated, so a mechanism that broke is as visible in the
        // audit trail as one that refused — otherwise the only trace of it would be the
        // absence of a decision, which is indistinguishable from never having asked.
        let decision = match policy.evaluate(&request, question.cx).await {
            Ok(decision) => decision,
            Err(error) => {
                self.record_decision(
                    question,
                    AuthorizationOutcome::PolicyUnavailable,
                    started.elapsed(),
                    None,
                );
                return Err(error);
            }
        };

        let (outcome, error, approval_wait) = match decision {
            Decision::Allow => (AuthorizationOutcome::Allowed, None, None),
            Decision::Deny { reason } => (
                AuthorizationOutcome::Denied {
                    reason: reason.clone(),
                },
                Some(format!("{subject}: {reason}")),
                None,
            ),
            Decision::RequireApproval { prompt } => match &self.approvals {
                None => (
                    AuthorizationOutcome::ApprovalUnavailable,
                    Some(format!(
                        "{subject} requires approval, but no approval sink is configured"
                    )),
                    None,
                ),
                Some(sink) => {
                    let approval_started = Instant::now();
                    match sink.request_approval(&request, &prompt, question.cx).await {
                        Ok(true) => (
                            AuthorizationOutcome::ApprovalGranted,
                            None,
                            Some(approval_started.elapsed()),
                        ),
                        Ok(false) => (
                            AuthorizationOutcome::ApprovalRefused,
                            Some(format!("{subject} was not approved")),
                            Some(approval_started.elapsed()),
                        ),
                        // Nobody to ask, nobody answered in time, the frontend went away:
                        // not a refusal by a human, and emphatically not an allow. The
                        // error itself is propagated unchanged, so a timeout stays a
                        // timeout for the caller.
                        Err(error) => {
                            self.record_decision(
                                question,
                                AuthorizationOutcome::ApprovalUnavailable,
                                started.elapsed(),
                                Some(approval_started.elapsed()),
                            );
                            return Err(error);
                        }
                    }
                }
            },
        };

        self.record_decision(question, outcome, started.elapsed(), approval_wait);

        match error {
            Some(message) => Err(Error::PermissionDenied(format!("tool `{tool}`: {message}"))),
            None => Ok(()),
        }
    }

    /// Runs both pre-execution authorization phases.
    async fn authorize(
        &self,
        cx: &ExecutionContext,
        spec: &ToolSpec,
        tool: &dyn Tool,
        principal: &Principal,
        arguments: &serde_json::Value,
    ) -> Result<()> {
        for action in &spec.required_permissions {
            self.decide(&Question {
                cx,
                tool: &spec.name,
                principal,
                action,
                resource: None,
                phase: AuthorizationPhase::Tool,
            })
            .await?;
        }

        for claim in tool.planned_resources(arguments)? {
            self.decide(&Question {
                cx,
                tool: &spec.name,
                principal,
                action: &claim.action,
                resource: Some(&claim.resource),
                phase: AuthorizationPhase::Resource,
            })
            .await?;
        }

        Ok(())
    }
}

/// One authorization question: who, what, on what, asked at which phase.
///
/// Bundling these keeps every phase asking a structurally identical question, so there is
/// no way for one of them to quietly omit, say, the principal.
struct Question<'a> {
    cx: &'a ExecutionContext,
    tool: &'a ToolName,
    principal: &'a Principal,
    action: &'a ActionId,
    resource: Option<&'a ResourceId>,
    phase: AuthorizationPhase,
}

impl Question<'_> {
    /// Renders the subject of the question for an error message.
    fn describe(&self) -> String {
        match self.resource {
            Some(resource) => format!("`{}` on `{resource}`", self.action),
            None => format!("`{}`", self.action),
        }
    }
}

/// The [`ResourceAuthorizer`] handed to a tool, bound to one invocation.
///
/// It captures the principal, correlation and tool identity of the call it was created
/// for, so a tool cannot ask on behalf of anyone else, about any other tool, or after the
/// call has finished — the borrow makes the last one a compile-time property.
struct ScopedAuthorizer<'a> {
    registry: &'a InProcessToolRegistry,
    tool: ToolName,
    principal: Principal,
    cx: &'a ExecutionContext,
}

impl std::fmt::Debug for ScopedAuthorizer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedAuthorizer")
            .field("tool", &self.tool)
            .field("correlation", &self.cx.correlation)
            .finish()
    }
}

#[async_trait]
impl ResourceAuthorizer for ScopedAuthorizer<'_> {
    async fn authorize(&self, action: &ActionId, resource: &ResourceId) -> Result<()> {
        self.registry
            .decide(&Question {
                cx: self.cx,
                tool: &self.tool,
                principal: &self.principal,
                action,
                resource: Some(resource),
                phase: AuthorizationPhase::DiscoveredResource,
            })
            .await
    }
}

#[async_trait]
impl ToolRegistry for InProcessToolRegistry {
    async fn list(&self, _cx: &ExecutionContext) -> Result<Vec<ToolSpec>> {
        let mut specs: Vec<ToolSpec> = self.tools.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs)
    }

    async fn invoke(
        &self,
        name: &ToolName,
        arguments: serde_json::Value,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let started = Instant::now();
        let principal = Self::principal_of(cx);

        let Some(tool) = self.tools.get(name).cloned() else {
            self.record_invocation(
                cx,
                name,
                &principal,
                InvocationOutcome::NotFound,
                started.elapsed(),
                None,
                None,
            );
            return Err(Error::not_found("tool", name));
        };

        let spec = tool.spec();
        let authorization_started = Instant::now();
        if let Err(error) = self
            .authorize(cx, &spec, tool.as_ref(), &principal, &arguments)
            .await
        {
            self.record_invocation(
                cx,
                name,
                &principal,
                InvocationOutcome::Denied,
                started.elapsed(),
                Some(authorization_started.elapsed()),
                None,
            );
            return Err(error);
        }
        let authorization_duration = authorization_started.elapsed();

        let authorizer = ScopedAuthorizer {
            registry: self,
            tool: name.clone(),
            principal: principal.clone(),
            cx,
        };

        let execution_started = Instant::now();
        let result = tool.invoke(arguments, &authorizer, cx).await;
        let execution_duration = execution_started.elapsed();

        let outcome = match &result {
            Ok(outcome) if outcome.is_error => InvocationOutcome::ReportedError,
            Ok(_) => InvocationOutcome::Succeeded,
            // A tool that refuses a discovered resource surfaces the denial here; record it
            // as denied rather than as a generic failure.
            Err(error) if error.kind() == aik_core::ErrorKind::Permission => {
                InvocationOutcome::Denied
            }
            Err(error) => InvocationOutcome::Failed {
                kind: format!("{:?}", error.kind()).to_lowercase(),
            },
        };
        self.record_invocation(
            cx,
            name,
            &principal,
            outcome,
            started.elapsed(),
            Some(authorization_duration),
            Some(execution_duration),
        );

        result
    }
}

/// Converts a duration to milliseconds, saturating rather than panicking on an
/// implausibly long one.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A [`PrincipalId`] for the implicit system principal, for policy engines that want to
/// match on it by name.
pub fn system_principal_id() -> PrincipalId {
    PrincipalId::new(SYSTEM_PRINCIPAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::tool::ResourceClaim;
    use serde_json::Value;

    struct Noop {
        name: &'static str,
        permissions: Vec<ActionId>,
    }

    #[async_trait]
    impl Tool for Noop {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: ToolName::new(self.name),
                description: "does nothing".into(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                required_permissions: self.permissions.clone(),
                read_only: true,
            }
        }

        async fn invoke(
            &self,
            _arguments: Value,
            _authorizer: &dyn ResourceAuthorizer,
            _cx: &ExecutionContext,
        ) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok(json!({})))
        }
    }

    /// Declares a resource claim whose construction fails, proving a bad claim stops the
    /// call before anything runs.
    struct BadClaim;

    #[async_trait]
    impl Tool for BadClaim {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: ToolName::new("bad"),
                description: "cannot describe its own resources".into(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                required_permissions: vec![],
                read_only: true,
            }
        }

        fn planned_resources(&self, _arguments: &Value) -> Result<Vec<ResourceClaim>> {
            Err(Error::InvalidArgument("unresolvable path".into()))
        }

        async fn invoke(
            &self,
            _arguments: Value,
            _authorizer: &dyn ResourceAuthorizer,
            _cx: &ExecutionContext,
        ) -> Result<ToolOutcome> {
            panic!("must not run when its resource claims could not be built");
        }
    }

    #[tokio::test]
    async fn registering_the_same_name_twice_fails() {
        let mut registry = InProcessToolRegistry::new();
        registry
            .register(Noop {
                name: "a",
                permissions: vec![],
            })
            .unwrap();

        let error = registry
            .register(Noop {
                name: "a",
                permissions: vec![],
            })
            .unwrap_err();
        assert!(matches!(error, Error::AlreadyExists { .. }), "{error}");
    }

    #[tokio::test]
    async fn invoking_an_unknown_tool_is_not_found() {
        let registry = InProcessToolRegistry::new();
        let error = registry
            .invoke(&ToolName::new("ghost"), json!({}), &ExecutionContext::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_tool_with_no_required_permissions_runs_with_no_policy_configured() {
        let mut registry = InProcessToolRegistry::new();
        registry
            .register(Noop {
                name: "free",
                permissions: vec![],
            })
            .unwrap();

        registry
            .invoke(&ToolName::new("free"), json!({}), &ExecutionContext::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_permissioned_tool_is_denied_with_no_policy_configured() {
        let mut registry = InProcessToolRegistry::new();
        registry
            .register(Noop {
                name: "guarded",
                permissions: vec![ActionId::new("demo.act")],
            })
            .unwrap();

        let error = registry
            .invoke(
                &ToolName::new("guarded"),
                json!({}),
                &ExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    }

    #[tokio::test]
    async fn a_tool_whose_resource_claims_cannot_be_built_never_runs() {
        let mut registry = InProcessToolRegistry::new();
        registry.register(BadClaim).unwrap();

        let error = registry
            .invoke(&ToolName::new("bad"), json!({}), &ExecutionContext::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }
}
