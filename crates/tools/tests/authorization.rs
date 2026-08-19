//! Tests for resource-level authorization and audit observability.
//!
//! These cover what the capability-level suite in `tools.rs` does not: the distinction
//! between "may this principal use this action?" and "…on this resource?", the third
//! phase for resources discovered mid-execution, and the audit events every decision
//! produces.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_api::audit::{
    AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase, InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, ApprovalSink, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalId,
    PrincipalKind, ResourceId,
};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolRegistry, ToolSpec};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use aik_tools::{EchoTool, ToolsComponent};
use async_trait::async_trait;
use serde_json::json;

const ACTION: &str = aik_tools::DEFAULT_PERMISSION;
const TOOL: &str = aik_tools::DEFAULT_NAME;

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

/// Allows the action outright, but only for resources under an allowed prefix.
///
/// Stands in for a real filesystem policy ("writes are fine inside the workspace") without
/// the kernel knowing anything about paths — as far as it is concerned these are opaque
/// strings.
struct PrefixPolicy {
    allowed_prefix: String,
    seen: Mutex<Vec<(ActionId, Option<ResourceId>)>>,
}

impl PrefixPolicy {
    fn new(allowed_prefix: &str) -> Self {
        Self {
            allowed_prefix: allowed_prefix.to_owned(),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn questions(&self) -> Vec<(ActionId, Option<ResourceId>)> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl PolicyEngine for PrefixPolicy {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        self.seen
            .lock()
            .unwrap()
            .push((request.action.clone(), request.resource.clone()));

        Ok(match &request.resource {
            // Capability-level question: the action itself is granted.
            None => Decision::Allow,
            // Resource-level question: only inside the allowed prefix.
            Some(resource) if resource.as_str().starts_with(&self.allowed_prefix) => {
                Decision::Allow
            }
            Some(resource) => Decision::deny(format!("`{resource}` is outside the workspace")),
        })
    }
}

/// Allows everything, recording each question.
struct AllowAll {
    seen: Mutex<Vec<PermissionRequest>>,
}

impl AllowAll {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<PermissionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl PolicyEngine for AllowAll {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        self.seen.lock().unwrap().push(request.clone());
        Ok(Decision::Allow)
    }
}

/// Defers every resource-level question to a human; allows capability-level ones.
struct AskForResources;

#[async_trait]
impl PolicyEngine for AskForResources {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        Ok(match &request.resource {
            None => Decision::Allow,
            Some(resource) => Decision::ask(format!("allow access to {resource}?")),
        })
    }
}

struct FixedApproval(bool);

#[async_trait]
impl ApprovalSink for FixedApproval {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        Ok(self.0)
    }
}

/// An approval sink that takes a while to answer, standing in for a human actually reading
/// the prompt — used to prove the wait is measured and reported separately from the rest of
/// the decision.
struct SlowApproval(Duration);

#[async_trait]
impl ApprovalSink for SlowApproval {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        tokio::time::sleep(self.0).await;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn agent(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::Agent))
}

/// Builds a started kernel with one `EchoTool` and the given policy.
async fn kernel_with(policy: Arc<dyn PolicyEngine>) -> Kernel {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    kernel
}

/// Collects everything currently buffered on a subscription, without waiting.
fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut collected = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        collected.push(envelope.payload);
    }
    collected
}

// ---------------------------------------------------------------------------
// Resource-level authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_allowed_action_on_an_allowed_resource_runs() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let outcome = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    assert_eq!(outcome.output["text"], json!("hi"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_same_action_on_a_forbidden_resource_is_denied() {
    // The crux: identical principal, identical action, different resource, opposite
    // outcome. Capability-level authorization alone could not express this.
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/etc/shadow" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(error.to_string().contains("/etc/shadow"), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn both_questions_are_asked_and_in_order() {
    let policy = Arc::new(PrefixPolicy::new("/workspace/"));
    let kernel = kernel_with(policy.clone()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let questions = policy.questions();
    assert_eq!(questions.len(), 2);
    // Capability level first, with no resource...
    assert_eq!(questions[0], (ActionId::new(ACTION), None));
    // ...then the specific resource.
    assert_eq!(
        questions[1],
        (ActionId::new(ACTION), Some(ResourceId::new("/workspace/a")))
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_denied_resource_stops_the_tool_before_it_runs() {
    // `delay_ms` would make the call observably slow if the tool had started; a prompt
    // denial proves execution never began.
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let started = tokio::time::Instant::now();
    let error = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/etc/shadow", "delay_ms": 30_000 }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tool_declaring_no_resource_asks_only_the_capability_question() {
    let policy = Arc::new(PrefixPolicy::new("/workspace/"));
    let kernel = kernel_with(policy.clone()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    tools
        .invoke(&ToolName::new(TOOL), json!({ "text": "hi" }), &agent("a1"))
        .await
        .unwrap();

    assert_eq!(policy.questions(), vec![(ActionId::new(ACTION), None)]);
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Phase three: resources discovered while running
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_resource_discovered_mid_run_is_authorized_through_the_same_policy() {
    let policy = Arc::new(PrefixPolicy::new("/workspace/"));
    let kernel = kernel_with(policy.clone()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "discovered_resource": "/workspace/found" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let questions = policy.questions();
    assert_eq!(
        questions.last().unwrap(),
        &(
            ActionId::new(ACTION),
            Some(ResourceId::new("/workspace/found"))
        )
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_forbidden_discovered_resource_aborts_the_running_tool() {
    // The TOCTOU case in miniature: the declared resource is fine, but what the tool
    // actually resolved to is not, and asking again is what stops it.
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({
                "text": "hi",
                "resource": "/workspace/link",
                "discovered_resource": "/etc/shadow"
            }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(error.to_string().contains("/etc/shadow"), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovered_resources_are_denied_when_no_policy_is_configured() {
    // Fail-closed must hold for the tool-initiated phase too, not just the registry's own.
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new().with_tool(EchoTool::new().requiring([])), // no capability check, no policy
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "discovered_resource": "/anything" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Audit events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_call_publishes_one_decision_per_question_and_one_invocation() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let cx = agent("a1");
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &cx,
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);

    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[0].action, ActionId::new(ACTION));
    assert_eq!(decided[0].resource, None);
    assert_eq!(decided[0].outcome, AuthorizationOutcome::Allowed);
    assert_eq!(decided[0].principal, PrincipalId::new("a1"));
    assert_eq!(decided[0].principal_kind, PrincipalKind::Agent);
    assert_eq!(decided[0].tool, ToolName::new(TOOL));
    assert_eq!(decided[0].correlation, cx.correlation);

    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(decided[1].resource, Some(ResourceId::new("/workspace/a")));

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::Succeeded);
    assert_eq!(invoked[0].correlation, cx.correlation);
    assert_eq!(invoked[0].tool, ToolName::new(TOOL));

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_denial_is_audited_with_the_policys_reason_and_a_denied_invocation() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/etc/shadow" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(
        decided[1].outcome,
        AuthorizationOutcome::Denied {
            reason: "`/etc/shadow` is outside the workspace".into()
        }
    );

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::Denied);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_discovered_resource_decision_is_audited_under_its_own_phase() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "discovered_resource": "/workspace/found" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    let last = decided.last().unwrap();
    assert_eq!(last.phase, AuthorizationPhase::DiscoveredResource);
    assert_eq!(last.resource, Some(ResourceId::new("/workspace/found")));
    assert_eq!(last.outcome, AuthorizationOutcome::Allowed);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_missing_policy_is_audited_as_a_misconfiguration_not_as_a_refusal() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(&ToolName::new(TOOL), json!({ "text": "hi" }), &agent("a1"))
        .await
        .unwrap_err();

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].outcome, AuthorizationOutcome::PolicyUnavailable);
    assert!(!decided[0].outcome.is_allowed());

    kernel.shutdown().await.unwrap();
}

/// A tool whose resource claim can never be constructed — stands in for a real tool (e.g. a
/// filesystem read) resolving a path that turns out not to exist. Never reached the policy
/// engine at all, so it must not be recorded as a policy refusal.
struct BadClaim;

#[async_trait]
impl Tool for BadClaim {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::new("bad-claim"),
            description: "cannot describe its own resources".into(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            required_permissions: vec![],
            read_only: true,
        }
    }

    fn planned_resources(&self, _arguments: &serde_json::Value) -> Result<Vec<ResourceClaim>> {
        Err(Error::not_found("file", "does-not-exist.txt"))
    }

    async fn invoke(
        &self,
        _arguments: serde_json::Value,
        _authorizer: &dyn aik_api::permission::ResourceAuthorizer,
        _cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        panic!("must not run when its resource claims could not be built");
    }
}

#[tokio::test]
async fn a_resource_claim_that_cannot_be_built_is_audited_as_failed_not_denied() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(BadClaim))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(&ToolName::new("bad-claim"), json!({}), &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error}");

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(
        invoked[0].outcome,
        InvocationOutcome::Failed {
            kind: "notfound".into()
        },
        "a resource claim that failed to resolve is not a policy decision — it never reached \
         one — so it must not be indistinguishable from an actual denial in the audit trail"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_outcomes_are_audited_distinctly() {
    for (approve, expected) in [
        (true, AuthorizationOutcome::ApprovalGranted),
        (false, AuthorizationOutcome::ApprovalRefused),
    ] {
        let kernel = Kernel::builder()
            .component(
                ToolsComponent::new()
                    .with_tool(EchoTool::new())
                    .with_policy(Arc::new(AskForResources))
                    .with_approvals(Arc::new(FixedApproval(approve))),
            )
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

        let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
        let _ = tools
            .invoke(
                &ToolName::new(TOOL),
                json!({ "text": "hi", "resource": "/workspace/a" }),
                &agent("a1"),
            )
            .await;

        let decided = drain(&mut decisions);
        assert_eq!(decided.last().unwrap().outcome, expected);
        assert!(
            decided.last().unwrap().approval_wait_ms.is_some(),
            "a decision that asked a sink should report how long it waited",
        );
        kernel.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn approval_wait_is_isolated_from_the_rest_of_the_decision() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(AskForResources))
                .with_approvals(Arc::new(SlowApproval(Duration::from_millis(50)))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    let resource_decision = decided
        .iter()
        .find(|event| event.phase == AuthorizationPhase::Resource)
        .unwrap();
    let wait = resource_decision.approval_wait_ms.unwrap();
    assert!(wait >= 50, "the sink slept 50ms: {wait}");
    // The total is at least the wait, since the wait is a subset of it — and this policy
    // engine's own evaluation is in-memory, so the two should be close rather than the total
    // dwarfing the wait.
    assert!(
        resource_decision.duration_ms >= wait,
        "total {} should be at least the wait {wait}",
        resource_decision.duration_ms,
    );

    // The tool-level decision never asked a sink at all, so it carries no wait.
    let tool_decision = decided
        .iter()
        .find(|event| event.phase == AuthorizationPhase::Tool)
        .unwrap();
    assert!(tool_decision.approval_wait_ms.is_none());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unavailable_approval_sink_is_audited_as_such() {
    let kernel = kernel_with(Arc::new(AskForResources)).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    let decided = drain(&mut decisions);
    assert_eq!(
        decided.last().unwrap().outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );
    assert!(
        decided.last().unwrap().approval_wait_ms.is_none(),
        "no sink was configured to ask, so no wait happened",
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_call_naming_an_unknown_tool_is_still_audited() {
    let kernel = kernel_with(Arc::new(AllowAll::new())).await;
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(&ToolName::new("probe.for.tools"), json!({}), &agent("a1"))
        .await
        .unwrap_err();

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::NotFound);
    assert_eq!(invoked[0].tool, ToolName::new("probe.for.tools"));

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_invocation_records_the_error_kind_not_its_message() {
    let kernel = kernel_with(Arc::new(AllowAll::new())).await;
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = agent("a1");
    let cancellation = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
    });

    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "delay_ms": 30_000 }),
            &cx,
        )
        .await
        .unwrap_err();

    let invoked = drain(&mut invocations);
    assert_eq!(
        invoked[0].outcome,
        InvocationOutcome::Failed {
            kind: "cancelled".into()
        }
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn audit_events_never_carry_tool_arguments_or_output() {
    // The single most important property of these events: they describe the shape of what
    // happened, never its contents. A real `filesystem.write` would put file bytes in its
    // arguments and a real `http.request` could put a token there.
    const SECRET: &str = "sk-super-secret-token-value";

    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut firehose = kernel.context().events().subscribe_any();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": SECRET, "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let mut saw_authorization = false;
    let mut saw_invocation = false;
    while let Some(Ok(envelope)) = firehose.try_recv() {
        let rendered = serde_json::to_string(&envelope).unwrap();
        assert!(
            !rendered.contains(SECRET),
            "event `{}` leaked tool arguments: {rendered}",
            envelope.metadata.name
        );
        match envelope.metadata.name.as_str() {
            "aik.authorization.decided" => saw_authorization = true,
            "aik.tool.invoked" => saw_invocation = true,
            _ => {}
        }
    }

    // Guard against the assertion above passing vacuously.
    assert!(saw_authorization && saw_invocation);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn audit_events_reach_an_out_of_process_style_subscriber_as_json() {
    // An audit sink should not need to link against `aik-api` to consume these.
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut firehose = kernel.context().events().subscribe_any();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let mut decisions = Vec::new();
    while let Some(Ok(envelope)) = firehose.try_recv() {
        if envelope.metadata.name.as_str() == "aik.authorization.decided" {
            decisions.push(envelope);
        }
    }

    assert_eq!(decisions.len(), 2);
    let resource_decision = &decisions[1];
    assert_eq!(resource_decision.payload["phase"], json!("resource"));
    assert_eq!(resource_decision.payload["outcome"], json!("allowed"));
    assert_eq!(resource_decision.payload["resource"], json!("/workspace/a"));
    assert_eq!(resource_decision.payload["principal"], json!("a1"));
    assert_eq!(resource_decision.payload["tool"], json!(TOOL));
    // The envelope carries provenance the payload does not need to repeat.
    assert_eq!(
        resource_decision.metadata.source,
        Some(ComponentId::new(aik_tools::DEFAULT_COMPONENT_ID))
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_decision_for_one_call_shares_its_correlation_id() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let first = agent("a1");
    let second = agent("a2");
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &first,
        )
        .await
        .unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/b" }),
            &second,
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    let invoked = drain(&mut invocations);

    // Two calls, two correlation ids, and every record joins to exactly one of them.
    assert_ne!(first.correlation, second.correlation);
    assert_eq!(
        decided
            .iter()
            .filter(|d| d.correlation == first.correlation)
            .count(),
        2
    );
    assert_eq!(
        decided
            .iter()
            .filter(|d| d.correlation == second.correlation)
            .count(),
        2
    );
    assert_eq!(invoked.len(), 2);
    assert_eq!(invoked[0].correlation, first.correlation);
    assert_eq!(invoked[1].correlation, second.correlation);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_context_with_no_principal_is_audited_as_the_system_principal() {
    let kernel = kernel_with(Arc::new(AllowAll::new())).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi" }),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    assert_eq!(decided[0].principal, aik_tools::system_principal_id());
    assert_eq!(decided[0].principal_kind, PrincipalKind::System);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn delegated_authority_is_visible_to_policy_and_in_the_audit_trail() {
    let policy = Arc::new(AllowAll::new());
    let kernel = kernel_with(policy.clone()).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let cx = ExecutionContext::new()
        .with_principal(Principal::new("agent-7", PrincipalKind::Agent).on_behalf_of("user-1"));
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(&ToolName::new(TOOL), json!({ "text": "hi" }), &cx)
        .await
        .unwrap();

    assert_eq!(
        policy.requests()[0].principal.on_behalf_of,
        Some(PrincipalId::new("user-1"))
    );

    let decided = drain(&mut decisions);
    assert_eq!(decided[0].on_behalf_of, Some(PrincipalId::new("user-1")));

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audit_events_do_not_cross_between_kernels() {
    let first = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let second = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;

    let mut second_decisions = second.context().subscribe::<AuthorizationDecided>();

    first
        .context()
        .service::<dyn ToolRegistry>()
        .unwrap()
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    assert!(drain(&mut second_decisions).is_empty());

    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// A mechanism that fails is not a mechanism that allowed
// ---------------------------------------------------------------------------

/// A policy engine that cannot answer at all.
struct BrokenPolicy;

#[async_trait]
impl PolicyEngine for BrokenPolicy {
    async fn evaluate(
        &self,
        _request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        Err(Error::other("the policy store is unreachable"))
    }
}

/// An approval sink that cannot reach anyone — the shape a real frontend takes when it has
/// gone away, timed out or was never attached.
struct BrokenApproval;

#[async_trait]
impl ApprovalSink for BrokenApproval {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        Err(Error::Timeout(Duration::from_secs(1)))
    }
}

#[tokio::test]
async fn a_policy_engine_that_fails_denies_and_is_audited_as_unavailable() {
    let kernel = kernel_with(Arc::new(BrokenPolicy)).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(&ToolName::new(TOOL), json!({ "text": "hi" }), &agent("a1"))
        .await
        .unwrap_err();

    // The engine's own error reaches the caller unchanged, rather than being flattened into
    // a denial that would hide a broken deployment.
    assert_eq!(error.kind(), aik_core::ErrorKind::Other, "{error}");

    // A failure to decide is still recorded as a decision point, so an audit trail cannot
    // silently omit the calls a broken policy engine was asked about.
    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[0].outcome, AuthorizationOutcome::PolicyUnavailable);
    assert!(!decided[0].outcome.is_allowed());

    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Denied
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_approval_sink_that_fails_denies_and_is_audited_as_unavailable() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(AskForResources))
                .with_approvals(Arc::new(BrokenApproval)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), aik_core::ErrorKind::Timeout, "{error}");

    // Distinct from `ApprovalRefused`: nobody said no, the question never got an answer.
    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(
        decided[1].outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );
    assert!(
        decided[1].approval_wait_ms.is_some(),
        "a sink was asked and failed, which is still a wait, however short",
    );

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Latency measurement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_invocation_reports_authorization_and_execution_durations() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert!(invoked[0].authorization_duration_ms.is_some());
    assert!(invoked[0].execution_duration_ms.is_some());
    // The total is at least as large as either phase alone, since it covers both.
    assert!(
        invoked[0].duration_ms >= invoked[0].authorization_duration_ms.unwrap()
            && invoked[0].duration_ms >= invoked[0].execution_duration_ms.unwrap()
    );
}

#[tokio::test]
async fn a_denied_invocation_has_no_execution_duration() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/etc/shadow" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert!(invoked[0].authorization_duration_ms.is_some());
    assert!(invoked[0].execution_duration_ms.is_none());
}

#[tokio::test]
async fn a_not_found_invocation_has_no_authorization_or_execution_duration() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(&ToolName::new("ghost"), json!({}), &agent("a1"))
        .await
        .unwrap_err();

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert!(invoked[0].authorization_duration_ms.is_none());
    assert!(invoked[0].execution_duration_ms.is_none());
}

#[tokio::test]
async fn every_authorization_decision_carries_a_duration() {
    let kernel = kernel_with(Arc::new(PrefixPolicy::new("/workspace/"))).await;
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new(TOOL),
            json!({ "text": "hi", "resource": "/workspace/a" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    for decision in decided {
        // Not asserting a lower bound beyond "measured at all": on a fast machine a
        // pure in-memory policy check can legitimately resolve in under a millisecond.
        let _ = decision.duration_ms;
    }
}
