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
use aik_api::provenance::{
    REACH_CONTEXT_KEY, Reach, SCOPE_ATTRIBUTE, TRUST_ATTRIBUTE, TRUST_CONTEXT_KEY, Trust,
    TrustLedger, TrustScope,
};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolRegistry, ToolSpec};
use aik_core::clock::{SharedClock, SystemClock};
use aik_core::event::{Envelope, Event, EventBus};
use aik_core::id::ComponentId;
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::trust::{InMemoryTrustLedger, TrustEnforcement, UNTRUSTED_CONTENT_ACTION};

/// The principal attributed to a call whose [`ExecutionContext`] names none.
///
/// The defaulting itself is
/// [`ExecutionContext::principal_or_system`](aik_api::execution::ExecutionContext::principal_or_system),
/// which every subsystem that owns resources shares; this is only the name, re-exported by
/// [`system_principal_id`] for policy engines that want to match on it.
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
    ledger: Arc<dyn TrustLedger>,
    enforcement: TrustEnforcement,
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
            .field("trust_enforcement", &self.enforcement)
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
            // Not an `Option`: a registry with no ledger would be one where provenance is
            // silently not tracked, and the whole point of the mechanism is that no
            // deployment has to remember to switch it on. Substituting a durable ledger is a
            // replacement, never an enabling.
            ledger: Arc::new(InMemoryTrustLedger::new()),
            enforcement: TrustEnforcement::default(),
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

    /// Replaces the [`TrustLedger`] this registry records and consults provenance in.
    ///
    /// Defaults to an [`InMemoryTrustLedger`], which forgets everything when the process
    /// exits; a durable one makes a resumed session resume the trust it actually has.
    #[must_use]
    pub fn with_trust_ledger(mut self, ledger: Arc<dyn TrustLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Sets how strictly provenance is enforced. Defaults to [`TrustEnforcement::Approval`].
    #[must_use]
    pub fn with_trust_enforcement(mut self, enforcement: TrustEnforcement) -> Self {
        self.enforcement = enforcement;
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
                scope_trust: Some(question.trust),
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
        output_trust: Option<Trust>,
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
                output_trust,
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

        let request = question.request();

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

    /// Runs both pre-execution authorization phases against an already-built set of resource
    /// claims.
    ///
    /// Takes `claims` rather than the tool and its arguments deliberately: building a claim
    /// (`Tool::planned_resources`) and asking whether it is allowed (`decide`) are different
    /// kinds of failure. Every error this method can return came from an actual authorization
    /// question being asked and refused — [`InProcessToolRegistry::invoke`] relies on that to
    /// tell "the tool was refused" apart from "the tool's resource claim could not even be
    /// built", which is not a policy decision at all. See [`classify_authorization_error`]'s
    /// documentation, and the [`decide`](Self::decide) doc comment for why even a broken
    /// policy engine or approval sink still counts as refused rather than merely failed.
    async fn authorize(
        &self,
        cx: &ExecutionContext,
        spec: &ToolSpec,
        principal: &Principal,
        claims: Vec<ResourceClaim>,
        trust: Trust,
    ) -> Result<()> {
        for action in &spec.required_permissions {
            self.decide(&Question {
                cx,
                tool: &spec.name,
                principal,
                action,
                resource: None,
                phase: AuthorizationPhase::Tool,
                trust,
                reach: spec.reach,
            })
            .await?;
        }

        for claim in claims {
            self.decide(&Question {
                cx,
                tool: &spec.name,
                principal,
                action: &claim.action,
                resource: Some(&claim.resource),
                phase: AuthorizationPhase::Resource,
                trust,
                reach: spec.reach,
            })
            .await?;
        }

        self.enforce_trust(cx, spec, principal, trust).await
    }

    /// Decides whether a conversation that has read untrusted content may act through this
    /// tool.
    ///
    /// Asked once per invocation, last, after identity-based authorization has already
    /// allowed the call — so this can only ever narrow what policy permitted, never widen it,
    /// and a call refused on identity is never additionally refused here. It is not a policy
    /// question and no rule is evaluated for it: [`TrustEnforcement`] decides, because the
    /// alternative is a mechanism every existing policy document would have to be rewritten
    /// to keep working, which is a mechanism most deployments would end up turning off.
    ///
    /// Nothing happens at all in the two cases that are not the trifecta: a conversation that
    /// has read nothing untrusted, and a tool that cannot carry anything out of one.
    async fn enforce_trust(
        &self,
        cx: &ExecutionContext,
        spec: &ToolSpec,
        principal: &Principal,
        trust: Trust,
    ) -> Result<()> {
        if !trust.is_untrusted() || spec.reach.is_contained() {
            return Ok(());
        }

        let started = Instant::now();
        let action = ActionId::new(UNTRUSTED_CONTENT_ACTION);
        let question = Question {
            cx,
            tool: &spec.name,
            principal,
            action: &action,
            resource: None,
            phase: AuthorizationPhase::Trust,
            trust,
            reach: spec.reach,
        };
        let tool = &spec.name;
        let effect = describe_reach(spec.reach);
        let reason =
            format!("this conversation has read untrusted content, and `{tool}` can {effect}");

        let (outcome, error, approval_wait) = match self.enforcement {
            TrustEnforcement::Observe => (AuthorizationOutcome::Allowed, None, None),
            TrustEnforcement::Deny => (
                AuthorizationOutcome::Denied {
                    reason: reason.clone(),
                },
                Some(reason.clone()),
                None,
            ),
            TrustEnforcement::Approval => match &self.approvals {
                None => (
                    AuthorizationOutcome::ApprovalUnavailable,
                    Some(format!("{reason}, and there is nobody to ask")),
                    None,
                ),
                Some(sink) => {
                    let prompt = format!(
                        "This conversation has read content from outside this deployment — a \
                         fetched page, a file, a program's output, or an external tool server. \
                         Running `{tool}` now would let that content {effect}. Untrusted \
                         content asking for exactly this is what a prompt injection looks \
                         like. Allow it?"
                    );
                    let approval_started = Instant::now();
                    match sink
                        .request_approval(&question.request(), &prompt, cx)
                        .await
                    {
                        Ok(true) => (
                            AuthorizationOutcome::ApprovalGranted,
                            None,
                            Some(approval_started.elapsed()),
                        ),
                        Ok(false) => (
                            AuthorizationOutcome::ApprovalRefused,
                            Some(format!("{reason}, and it was not approved")),
                            Some(approval_started.elapsed()),
                        ),
                        Err(error) => {
                            self.record_decision(
                                &question,
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

        self.record_decision(&question, outcome, started.elapsed(), approval_wait);

        match error {
            Some(message) => Err(Error::PermissionDenied(format!("tool `{tool}`: {message}"))),
            None => Ok(()),
        }
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
    /// What the conversation this call belongs to has read so far.
    trust: Trust,
    /// How far the tool being asked about can reach.
    reach: Reach,
}

impl Question<'_> {
    /// Renders the subject of the question for an error message.
    fn describe(&self) -> String {
        match self.resource {
            Some(resource) => format!("`{}` on `{resource}`", self.action),
            None => format!("`{}`", self.action),
        }
    }

    /// The request put to the policy engine, and to a human if it defers to one.
    ///
    /// Trust and reach travel in the context rather than as fields of
    /// [`PermissionRequest`], for the same reason the tool name does: they are facts a rule
    /// may want to constrain on, not part of the question's identity. A policy rule reads
    /// them through its `context` matcher — see [`aik_api::provenance`].
    fn request(&self) -> PermissionRequest {
        let mut context = Map::new();
        context.insert("tool".to_owned(), Value::from(self.tool.as_str()));
        context.insert(
            TRUST_CONTEXT_KEY.to_owned(),
            Value::from(self.trust.as_str()),
        );
        context.insert(
            REACH_CONTEXT_KEY.to_owned(),
            Value::from(self.reach.as_str()),
        );
        PermissionRequest {
            principal: self.principal.clone(),
            action: self.action.clone(),
            resource: self.resource.cloned(),
            context: Value::Object(context),
        }
    }
}

/// The scope whose trust a call inherits.
///
/// The session where there is one, because the transcript is what carries untrusted text
/// from one turn to the next, and the operation's correlation otherwise. The two are
/// prefixed so that no session id can ever name the same scope as some correlation.
///
/// The attribute is written by the agent loop from the caller's session id and is not
/// reachable by a model; see [`aik_api::agent::SESSION_ATTRIBUTE`].
fn scope_of(cx: &ExecutionContext) -> TrustScope {
    match cx.attributes.get(SCOPE_ATTRIBUTE).and_then(Value::as_str) {
        Some(session) if !session.is_empty() => TrustScope::new(format!("session:{session}")),
        _ => TrustScope::new(format!("operation:{}", cx.correlation)),
    }
}

/// What a tool of this reach can do, in a sentence a person is being asked to judge.
fn describe_reach(reach: Reach) -> &'static str {
    match reach {
        Reach::Contained => "read state this deployment already holds",
        Reach::Mutating => "change state on this machine",
        Reach::External => "send data outside this deployment",
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
    /// The scope's trust as it stood when the call was authorized.
    ///
    /// Fixed for the duration of the invocation, like everything else here. A tool cannot
    /// change what its conversation has read, and a call that was allowed to start does not
    /// become a different question halfway through.
    trust: Trust,
    reach: Reach,
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
                trust: self.trust,
                reach: self.reach,
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
        let principal = cx.principal_or_system();

        let Some(tool) = self.tools.get(name).cloned() else {
            self.record_invocation(
                cx,
                name,
                &principal,
                InvocationOutcome::NotFound,
                None,
                started.elapsed(),
                None,
                None,
            );
            return Err(Error::not_found("tool", name));
        };

        let spec = tool.spec();
        let authorization_started = Instant::now();

        // What this conversation has already been told, read once and used for every question
        // this call asks. A ledger that cannot answer is treated the way an unavailable policy
        // engine is: the call stops. "We could not find out whether this conversation is
        // tainted" is not "it is not".
        let scope = scope_of(cx);
        let trust = match self.ledger.trust_of(&scope).await {
            Ok(trust) => trust,
            Err(error) => {
                self.record_invocation(
                    cx,
                    name,
                    &principal,
                    InvocationOutcome::Denied,
                    None,
                    started.elapsed(),
                    Some(authorization_started.elapsed()),
                    None,
                );
                return Err(Error::PermissionDenied(format!(
                    "tool `{name}`: the trust of this conversation could not be established: {error}"
                )));
            }
        };

        // Building a resource claim is not an authorization question — no `decide` was ever
        // asked, so a failure here (e.g. a path that does not resolve) is not a policy
        // decision and must not be recorded as one.
        let claims = match tool.planned_resources(&arguments) {
            Ok(claims) => claims,
            Err(error) => {
                self.record_invocation(
                    cx,
                    name,
                    &principal,
                    InvocationOutcome::Failed {
                        kind: format!("{:?}", error.kind()).to_lowercase(),
                    },
                    None,
                    started.elapsed(),
                    Some(authorization_started.elapsed()),
                    None,
                );
                return Err(error);
            }
        };

        if let Err(error) = self.authorize(cx, &spec, &principal, claims, trust).await {
            self.record_invocation(
                cx,
                name,
                &principal,
                InvocationOutcome::Denied,
                None,
                started.elapsed(),
                Some(authorization_started.elapsed()),
                None,
            );
            return Err(error);
        }
        let authorization_duration = authorization_started.elapsed();

        // The tool runs under a context carrying the scope's trust, so a tool that persists
        // what it was given — a memory store, say — can record where it came from without
        // resolving a ledger of its own. It is annotation, not authority: nothing read back
        // from here decides anything.
        let cx = &cx.child().with_attribute(TRUST_ATTRIBUTE, trust.as_str());

        let authorizer = ScopedAuthorizer {
            registry: self,
            tool: name.clone(),
            principal: principal.clone(),
            cx,
            trust,
            reach: spec.reach,
        };

        let execution_started = Instant::now();
        let result = tool.invoke(arguments, &authorizer, cx).await;
        let execution_duration = execution_started.elapsed();

        // What the tool declared it can return, narrowed by what this particular call says it
        // did return. Only a result that actually reaches the caller counts: a refused or
        // failed call put nothing into the conversation.
        let output_trust = result
            .as_ref()
            .ok()
            .map(|outcome| spec.output_trust.min_with(outcome.trust));

        let outcome = match &result {
            Ok(outcome) if outcome.is_error => InvocationOutcome::ReportedError,
            Ok(_) => InvocationOutcome::Succeeded,
            // A tool that refuses a discovered resource surfaces the denial here; record it
            // as denied rather than as a generic failure.
            Err(error) => classify_authorization_error(error),
        };

        // Recorded before the result is handed back, and a failure to record discards it. A
        // ledger that missed a taint is a conversation that has read an untrusted page and
        // has nothing to show for it, which is worse than a tool call that failed: the caller
        // can retry a failure.
        let ledger_failure = match output_trust {
            Some(trust) => self.ledger.observe(&scope, trust).await.err(),
            None => None,
        };

        let (outcome, output_trust) = match &ledger_failure {
            None => (outcome, output_trust),
            Some(error) => (
                InvocationOutcome::Failed {
                    kind: format!("{:?}", error.kind()).to_lowercase(),
                },
                None,
            ),
        };

        self.record_invocation(
            cx,
            name,
            &principal,
            outcome,
            output_trust,
            started.elapsed(),
            Some(authorization_duration),
            Some(execution_duration),
        );

        match ledger_failure {
            None => result,
            Some(error) => Err(Error::other(format!(
                "tool `{name}` ran, but its result was discarded: the provenance of what it \
                 returned could not be recorded: {error}"
            ))),
        }
    }
}

/// Converts a duration to milliseconds, saturating rather than panicking on an
/// implausibly long one.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Classifies an error returned by [`Tool::invoke`] as [`InvocationOutcome::Denied`] or
/// [`InvocationOutcome::Failed`].
///
/// Used only for a tool's own execution result, where — unlike the two pre-execution phases
/// in [`InProcessToolRegistry::invoke`], which know structurally whether they are reporting a
/// refused claim or a decision — there is no such structural signal: a running tool can fail
/// for its own reasons (cancellation, timeout, I/O) or because a
/// [`ResourceAuthorizer::authorize`](aik_api::permission::ResourceAuthorizer::authorize) call
/// it made mid-run was refused. [`aik_core::ErrorKind::Permission`] is the best available
/// signal for the latter.
fn classify_authorization_error(error: &Error) -> InvocationOutcome {
    if error.kind() == aik_core::ErrorKind::Permission {
        InvocationOutcome::Denied
    } else {
        InvocationOutcome::Failed {
            kind: format!("{:?}", error.kind()).to_lowercase(),
        }
    }
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
    use serde_json::{Value, json};

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
                output_trust: Trust::Trusted,
                reach: Reach::Contained,
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
                output_trust: Trust::Trusted,
                reach: Reach::Contained,
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
