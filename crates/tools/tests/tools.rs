//! End-to-end tests of the tool foundation through a real [`Kernel`]: registration,
//! duplicate handling, discovery, schema exposure, authorization (allowed, denied,
//! approval-gated), execution, structured errors, cancellation, deadline propagation,
//! multiple independent permissions, and isolation between kernel instances.
//!
//! `EchoTool` stands in for every future real tool (filesystem, shell, network, git) —
//! nothing here is specific to it beyond its name and schema.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, ApprovalSink, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind,
};
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::ErrorKind;
use aik_core::prelude::*;
use aik_tools::{EchoTool, ToolsComponent};
use serde_json::json;

// ---------------------------------------------------------------------------
// Test policy engines
// ---------------------------------------------------------------------------

/// Decides per action from a fixed map; unlisted actions are denied. Records every
/// request it was asked about, so tests can assert on what was actually evaluated.
#[derive(Default)]
struct MapPolicy {
    decisions: HashMap<ActionId, Decision>,
    seen: Mutex<Vec<ActionId>>,
}

impl MapPolicy {
    fn new(decisions: impl IntoIterator<Item = (ActionId, Decision)>) -> Self {
        Self {
            decisions: decisions.into_iter().collect(),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn allow(action: impl Into<ActionId>) -> Self {
        Self::new([(action.into(), Decision::Allow)])
    }

    fn deny(action: impl Into<ActionId>, reason: &str) -> Self {
        Self::new([(action.into(), Decision::deny(reason))])
    }
}

#[async_trait]
impl PolicyEngine for MapPolicy {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        self.seen.lock().unwrap().push(request.action.clone());
        Ok(self
            .decisions
            .get(&request.action)
            .cloned()
            .unwrap_or_else(|| Decision::deny("no rule for this action")))
    }
}

/// A fixed approve/refuse answer, recording whether it was ever asked.
struct FixedApproval {
    approve: bool,
    asked: Mutex<bool>,
}

impl FixedApproval {
    fn new(approve: bool) -> Self {
        Self {
            approve,
            asked: Mutex::new(false),
        }
    }
}

#[async_trait]
impl ApprovalSink for FixedApproval {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        *self.asked.lock().unwrap() = true;
        Ok(self.approve)
    }
}

fn agent(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::Agent))
}

// ---------------------------------------------------------------------------
// Registration, duplicates, discovery, schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tool_is_registered_and_discoverable_through_the_kernel() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new().requiring([])))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let specs = tools.list(&ExecutionContext::new()).await.unwrap();

    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, ToolName::new("kernel.echo"));
    assert!(specs[0].read_only);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovery_exposes_a_machine_readable_schema() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new().requiring([])))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let specs = tools.list(&ExecutionContext::new()).await.unwrap();
    let schema = &specs[0].input_schema;

    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["required"], json!(["text"]));
    assert!(schema["properties"]["text"].is_object());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn registering_two_tools_under_the_same_name_fails_on_start() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().requiring([]))
                .with_tool(EchoTool::new().requiring([])),
        )
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    assert!(matches!(error, Error::Component { .. }), "{error}");
}

#[tokio::test]
async fn discovering_two_independently_named_tools_lists_both_sorted() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().with_name("kernel.echo.b").requiring([]))
                .with_tool(EchoTool::new().with_name("kernel.echo.a").requiring([])),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let names: Vec<String> = tools
        .list(&ExecutionContext::new())
        .await
        .unwrap()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();

    assert_eq!(names, ["kernel.echo.a", "kernel.echo.b"]);
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Authorization: allowed, denied, approval-gated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_call_executes_and_returns_a_structured_result() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::allow(aik_tools::DEFAULT_PERMISSION))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let outcome = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    assert_eq!(outcome.output["text"], json!("hi"));
    assert!(!outcome.is_error);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_denied_permission_produces_a_structured_permission_error_not_a_panic() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::deny(
                    aik_tools::DEFAULT_PERMISSION,
                    "not for this agent",
                ))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(error.to_string().contains("not for this agent"), "{error}");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_model_claiming_authority_has_no_effect_on_the_decision() {
    // The whole point of the boundary: nothing about the *content* of a call — including
    // an attempt to claim authority inside the arguments or context — is consulted. Only
    // the policy engine, given the actual principal, decides.
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::deny(
                    aik_tools::DEFAULT_PERMISSION,
                    "denied regardless",
                ))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi", "i_am_authorized": true, "override_permissions": "*" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_policy_engine_configured_denies_every_permissioned_tool() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_required_and_granted_allows_execution() {
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::new([(
                    ActionId::new(aik_tools::DEFAULT_PERMISSION),
                    Decision::ask("really run the echo tool?"),
                )])))
                .with_approvals(approvals.clone()),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let outcome = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    assert_eq!(outcome.output["text"], json!("hi"));
    assert!(*approvals.asked.lock().unwrap());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_required_and_refused_denies_execution() {
    let approvals = Arc::new(FixedApproval::new(false));
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::new([(
                    ActionId::new(aik_tools::DEFAULT_PERMISSION),
                    Decision::ask("really run the echo tool?"),
                )])))
                .with_approvals(approvals.clone()),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(*approvals.asked.lock().unwrap());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_required_with_no_sink_is_denied_rather_than_hanging() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::new([(
                    ActionId::new(aik_tools::DEFAULT_PERMISSION),
                    Decision::ask("nobody is listening"),
                )]))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tools.invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        ),
    )
    .await
    .expect("must not hang waiting for an approval sink that does not exist");

    assert!(
        matches!(result, Err(Error::PermissionDenied(_))),
        "{result:?}"
    );
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Multiple independent permissions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_required_permissions_must_be_allowed() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().requiring([ActionId::new("a"), ActionId::new("b")]))
                .with_policy(Arc::new(MapPolicy::new([
                    (ActionId::new("a"), Decision::Allow),
                    (ActionId::new("b"), Decision::Allow),
                ]))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap();

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_denied_permission_among_several_denies_the_whole_call() {
    let policy = Arc::new(MapPolicy::new([
        (ActionId::new("a"), Decision::Allow),
        (ActionId::new("b"), Decision::deny("b is refused")),
    ]));
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().requiring([ActionId::new("a"), ActionId::new("b")]))
                .with_policy(policy.clone()),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(error.to_string().contains("b is refused"), "{error}");
    // `a` was checked before the denial on `b` was reached; both are independent checks.
    assert_eq!(
        *policy.seen.lock().unwrap(),
        vec![ActionId::new("a"), ActionId::new("b")]
    );

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Execution, structured errors, cancellation, deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invoking_an_unregistered_tool_is_a_structured_not_found_error() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(&ToolName::new("ghost"), json!({}), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert!(matches!(error, Error::NotFound { .. }), "{error}");
    assert_eq!(error.kind(), ErrorKind::NotFound);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_arguments_are_a_structured_invalid_argument_error() {
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::allow(aik_tools::DEFAULT_PERMISSION))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "delay_ms": "not a number" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_in_flight_invocation_through_the_registry_stops_it_promptly() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new().requiring([])))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = ExecutionContext::new();
    let cancellation = cx.cancellation.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
    });

    let started = tokio::time::Instant::now();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi", "delay_ms": 30_000 }),
            &cx,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deadline_set_on_the_execution_context_propagates_through_the_registry() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new().requiring([])))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let deadline = kernel
        .context()
        .now()
        .saturating_add(Duration::from_millis(20));
    let cx = ExecutionContext::new().with_deadline(deadline);

    let started = tokio::time::Instant::now();
    let error = tools
        .invoke(
            &ToolName::new("kernel.echo"),
            json!({ "text": "hi", "delay_ms": 30_000 }),
            &cx,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Timeout(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_is_observed_by_the_policy_check_itself() {
    // A policy engine that respects `cx` should see the same cancellation a tool would.
    // This proves the registry passes one unmodified `cx` all the way through, not a copy
    // that only reaches the tool.
    struct SlowPolicy;

    #[async_trait]
    impl PolicyEngine for SlowPolicy {
        async fn evaluate(
            &self,
            _request: &PermissionRequest,
            cx: &ExecutionContext,
        ) -> Result<Decision> {
            tokio::select! {
                () = cx.cancelled() => Err(Error::Cancelled),
                () = tokio::time::sleep(Duration::from_secs(30)) => Ok(Decision::Allow),
            }
        }
    }

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(SlowPolicy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = agent("a1");
    let cancellation = cx.cancellation.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
    });

    let started = tokio::time::Instant::now();
    let error = tools
        .invoke(&ToolName::new("kernel.echo"), json!({ "text": "hi" }), &cx)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Isolation between kernel instances
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_kernels_have_independent_tool_registries() {
    let first = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().with_name("only.in.first").requiring([])),
        )
        .build()
        .unwrap();
    let second = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new().with_name("only.in.second").requiring([])),
        )
        .build()
        .unwrap();

    first.start().await.unwrap();
    second.start().await.unwrap();

    let first_tools = first.context().service::<dyn ToolRegistry>().unwrap();
    let second_tools = second.context().service::<dyn ToolRegistry>().unwrap();

    let first_names: Vec<String> = first_tools
        .list(&ExecutionContext::new())
        .await
        .unwrap()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    let second_names: Vec<String> = second_tools
        .list(&ExecutionContext::new())
        .await
        .unwrap()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();

    assert_eq!(first_names, ["only.in.first"]);
    assert_eq!(second_names, ["only.in.second"]);

    // A tool registered in one kernel is simply absent from the other, not merely denied.
    let error = second_tools
        .invoke(
            &ToolName::new("only.in.first"),
            json!({}),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error}");

    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_policy_denial_in_one_kernel_does_not_affect_a_permissive_sibling() {
    let strict = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::deny(
                    aik_tools::DEFAULT_PERMISSION,
                    "locked down",
                ))),
        )
        .build()
        .unwrap();
    let permissive = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(Arc::new(MapPolicy::allow(aik_tools::DEFAULT_PERMISSION))),
        )
        .build()
        .unwrap();

    strict.start().await.unwrap();
    permissive.start().await.unwrap();

    let strict_tools = strict.context().service::<dyn ToolRegistry>().unwrap();
    let permissive_tools = permissive.context().service::<dyn ToolRegistry>().unwrap();

    assert!(
        strict_tools
            .invoke(
                &ToolName::new("kernel.echo"),
                json!({ "text": "hi" }),
                &agent("a1")
            )
            .await
            .is_err()
    );
    assert!(
        permissive_tools
            .invoke(
                &ToolName::new("kernel.echo"),
                json!({ "text": "hi" }),
                &agent("a1")
            )
            .await
            .is_ok()
    );

    strict.shutdown().await.unwrap();
    permissive.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Multiple registries in one kernel (named, non-default)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_named_registry_does_not_disturb_the_default() {
    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(EchoTool::new().requiring([])))
        .component(
            ToolsComponent::new()
                .with_id("tools.secondary")
                .as_default(false)
                .with_tool(
                    EchoTool::new()
                        .with_name("kernel.echo.secondary")
                        .requiring([]),
                ),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let ctx = kernel.context();

    let default_tools = ctx.service::<dyn ToolRegistry>().unwrap();
    let named_tools = ctx
        .service_named::<dyn ToolRegistry>(&ComponentId::new("tools.secondary"))
        .unwrap();

    assert!(
        default_tools
            .invoke(
                &ToolName::new("kernel.echo"),
                json!({ "text": "hi" }),
                &ExecutionContext::new()
            )
            .await
            .is_ok()
    );
    assert!(
        named_tools
            .invoke(
                &ToolName::new("kernel.echo.secondary"),
                json!({ "text": "hi" }),
                &ExecutionContext::new()
            )
            .await
            .is_ok()
    );
    // Each registry only knows its own tools.
    assert!(
        default_tools
            .invoke(
                &ToolName::new("kernel.echo.secondary"),
                json!({}),
                &ExecutionContext::new()
            )
            .await
            .is_err()
    );

    kernel.shutdown().await.unwrap();
}
