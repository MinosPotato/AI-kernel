//! Tests for the fourth question the registry asks: what has this conversation read?
//!
//! The three phases covered by `authorization.rs` all ask what a *principal* may do. These
//! cover the one that asks what a *conversation* has been told, and what the registry does
//! about a call that would let somebody else's text act.

use std::sync::{Arc, Mutex};

use aik_api::audit::{AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase, ToolInvoked};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, ApprovalSink, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind,
    ResourceAuthorizer,
};
use aik_api::provenance::{
    Reach, SCOPE_ATTRIBUTE, TRUST_ATTRIBUTE, TRUST_CONTEXT_KEY, Trust, TrustLedger, TrustScope,
};
use aik_api::tool::{Tool, ToolName, ToolOutcome, ToolRegistry, ToolSpec};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use aik_tools::{TrustEnforcement, UNTRUSTED_CONTENT_ACTION};
use async_trait::async_trait;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A tool that declares whatever this test needs it to declare.
struct Declared {
    name: &'static str,
    output_trust: Trust,
    reach: Reach,
    /// Set on the result of each individual call, narrowing the declaration above.
    per_call_trust: Trust,
    /// Every execution context this tool was handed, for inspection afterwards.
    seen: Arc<Mutex<Vec<ExecutionContext>>>,
}

impl Declared {
    fn new(name: &'static str, output_trust: Trust, reach: Reach) -> Self {
        Self {
            name,
            output_trust,
            reach,
            per_call_trust: Trust::Trusted,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A tool whose declaration is trusted but whose results are not.
    fn narrowing(name: &'static str, reach: Reach) -> Self {
        Self {
            per_call_trust: Trust::Untrusted,
            ..Self::new(name, Trust::Trusted, reach)
        }
    }

    fn observations(&self) -> Arc<Mutex<Vec<ExecutionContext>>> {
        self.seen.clone()
    }
}

#[async_trait]
impl Tool for Declared {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::new(self.name),
            description: "declares its provenance".into(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            required_permissions: vec![ActionId::new("demo.act")],
            read_only: true,
            output_trust: self.output_trust,
            reach: self.reach,
        }
    }

    async fn invoke(
        &self,
        _arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        self.seen.lock().unwrap().push(cx.clone());
        Ok(ToolOutcome::ok(json!({ "ran": self.name })).with_trust(self.per_call_trust))
    }
}

// ---------------------------------------------------------------------------
// Collaborators
// ---------------------------------------------------------------------------

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

/// Answers every approval the same way, recording what it was shown.
struct FixedApproval {
    answer: bool,
    prompts: Mutex<Vec<(PermissionRequest, String)>>,
}

impl FixedApproval {
    fn new(answer: bool) -> Self {
        Self {
            answer,
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<(PermissionRequest, String)> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalSink for FixedApproval {
    async fn request_approval(
        &self,
        request: &PermissionRequest,
        prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        self.prompts
            .lock()
            .unwrap()
            .push((request.clone(), prompt.to_owned()));
        Ok(self.answer)
    }
}

/// A ledger that cannot answer, standing in for a durable one whose database is unreachable.
struct BrokenLedger;

#[async_trait]
impl TrustLedger for BrokenLedger {
    async fn observe(&self, _scope: &TrustScope, _trust: Trust) -> Result<()> {
        Err(Error::other("the ledger is unreachable"))
    }

    async fn trust_of(&self, _scope: &TrustScope) -> Result<Trust> {
        Err(Error::other("the ledger is unreachable"))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A context standing in for one conversation, the way the agent loop annotates one.
fn session(name: &str) -> ExecutionContext {
    ExecutionContext::new()
        .with_principal(Principal::new("assistant", PrincipalKind::Agent))
        .with_attribute(SCOPE_ATTRIBUTE, name)
}

fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut collected = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        collected.push(envelope.payload);
    }
    collected
}

/// A kernel holding a reader (untrusted output, contained) and an actor (trusted output,
/// external), which is the smallest shape the mechanism is about.
async fn kernel_with(
    enforcement: TrustEnforcement,
    approvals: Option<Arc<dyn ApprovalSink>>,
    policy: Arc<dyn PolicyEngine>,
) -> Kernel {
    let mut tools = aik_tools::ToolsComponent::new()
        .with_policy(policy)
        .with_trust_enforcement(enforcement)
        .with_tool(Declared::new("reader", Trust::Untrusted, Reach::Contained))
        .with_tool(Declared::new("clean", Trust::Trusted, Reach::Contained))
        .with_tool(Declared::new("writer", Trust::Trusted, Reach::Mutating))
        .with_tool(Declared::new("sender", Trust::Trusted, Reach::External));
    if let Some(approvals) = approvals {
        tools = tools.with_approvals(approvals);
    }
    let kernel = Kernel::builder().component(tools).build().unwrap();
    kernel.start().await.unwrap();
    kernel
}

async fn run(tools: &Arc<dyn ToolRegistry>, name: &str, cx: &ExecutionContext) -> Result<()> {
    tools.invoke(&ToolName::new(name), json!({}), cx).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_untainted_conversation_acts_without_being_asked_about() {
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Approval,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "clean", &cx).await.unwrap();
    run(&tools, "sender", &cx).await.unwrap();

    assert!(
        approvals.asked().is_empty(),
        "nothing untrusted was read, so nothing should have been escalated"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn reading_untrusted_content_puts_the_next_acting_call_in_front_of_a_human() {
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Approval,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "writer", &cx).await.unwrap();

    let asked = approvals.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let (request, prompt) = &asked[0];
    assert_eq!(request.action, ActionId::new(UNTRUSTED_CONTENT_ACTION));
    assert_eq!(request.context[TRUST_CONTEXT_KEY], json!("untrusted"));
    assert!(prompt.contains("`writer`"), "{prompt}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_human_who_refuses_stops_the_call() {
    let approvals = Arc::new(FixedApproval::new(false));
    let kernel = kernel_with(
        TrustEnforcement::Approval,
        Some(approvals),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    let error = run(&tools, "sender", &cx).await.unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn with_nobody_to_ask_the_call_is_refused_rather_than_allowed() {
    // The unattended deployment: `Approval` is still fail-closed, because the fallback when
    // there is no sink is a denial and not a pass-through.
    let kernel = kernel_with(TrustEnforcement::Approval, None, Arc::new(AllowAll::new())).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    let error = run(&tools, "writer", &cx).await.unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_contained_tool_is_never_gated_however_tainted_the_conversation_is() {
    // Reading more untrusted content is not the danger the gate is about, and treating it as
    // one would make an assistant unable to read a second file after reading a first.
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Deny,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "clean", &cx).await.unwrap();

    assert!(approvals.asked().is_empty());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn deny_refuses_without_asking_anybody() {
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Deny,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    let error = run(&tools, "writer", &cx).await.unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(
        approvals.asked().is_empty(),
        "a deployment that says deny is not asking"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn observe_allows_and_still_records() {
    let kernel = kernel_with(TrustEnforcement::Observe, None, Arc::new(AllowAll::new())).await;
    let mut decisions = kernel
        .context()
        .events()
        .subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "writer", &cx).await.unwrap();

    let trust: Vec<AuthorizationDecided> = drain(&mut decisions)
        .into_iter()
        .filter(|decision| decision.phase == AuthorizationPhase::Trust)
        .collect();
    assert_eq!(trust.len(), 1, "{trust:?}");
    assert_eq!(trust[0].outcome, AuthorizationOutcome::Allowed);
    assert_eq!(trust[0].scope_trust, Some(Trust::Untrusted));
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// What taint is, and is not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn taint_does_not_cross_between_conversations() {
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Approval,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    run(&tools, "reader", &session("s1")).await.unwrap();
    run(&tools, "writer", &session("s2")).await.unwrap();

    assert!(
        approvals.asked().is_empty(),
        "one conversation's reading must not constrain another's"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn taint_survives_across_the_turns_of_one_conversation() {
    // The property the session scope exists for: the fetched page is still in the window
    // three turns later, so a scope that reset per operation would forget exactly when it
    // matters. Each call here gets a fresh correlation id and the same session.
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = kernel_with(
        TrustEnforcement::Approval,
        Some(approvals.clone()),
        Arc::new(AllowAll::new()),
    )
    .await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    run(&tools, "reader", &session("s1")).await.unwrap();
    run(&tools, "clean", &session("s1")).await.unwrap();
    run(&tools, "writer", &session("s1")).await.unwrap();

    assert_eq!(approvals.asked().len(), 1);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_call_can_narrow_its_own_trust_below_what_its_tool_declared() {
    // What `memory.query` needs: a tool whose output is usually this deployment's own, but
    // is not for this particular result.
    let approvals = Arc::new(FixedApproval::new(true));
    let kernel = Kernel::builder()
        .component(
            aik_tools::ToolsComponent::new()
                .with_policy(Arc::new(AllowAll::new()))
                .with_approvals(approvals.clone() as Arc<dyn ApprovalSink>)
                .with_tool(Declared::narrowing("recall", Reach::Contained))
                .with_tool(Declared::new("writer", Trust::Trusted, Reach::Mutating)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "recall", &cx).await.unwrap();
    run(&tools, "writer", &cx).await.unwrap();

    assert_eq!(approvals.asked().len(), 1);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_trust_of_a_conversation_reaches_the_tool_it_runs() {
    let reader = Declared::new("reader", Trust::Untrusted, Reach::Contained);
    let writer = Declared::new("writer", Trust::Trusted, Reach::Mutating);
    let observed = writer.observations();
    let kernel = Kernel::builder()
        .component(
            aik_tools::ToolsComponent::new()
                .with_policy(Arc::new(AllowAll::new()))
                .with_trust_enforcement(TrustEnforcement::Observe)
                .with_tool(reader)
                .with_tool(writer),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "writer", &cx).await.unwrap();
    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "writer", &cx).await.unwrap();

    let seen: Vec<Value> = observed
        .lock()
        .unwrap()
        .iter()
        .map(|cx| cx.attributes[TRUST_ATTRIBUTE].clone())
        .collect();
    assert_eq!(seen, [json!("trusted"), json!("untrusted")]);
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// What policy and the audit trail see
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_policy_question_carries_the_conversation_s_trust_and_the_tool_s_reach() {
    let policy = Arc::new(AllowAll::new());
    let kernel = kernel_with(TrustEnforcement::Observe, None, policy.clone()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "reader", &cx).await.unwrap();
    run(&tools, "sender", &cx).await.unwrap();

    let requests = policy.requests();
    assert_eq!(requests[0].context["aik.trust"], json!("trusted"));
    assert_eq!(requests[0].context["aik.reach"], json!("contained"));
    assert_eq!(requests[1].context["aik.trust"], json!("untrusted"));
    assert_eq!(requests[1].context["aik.reach"], json!("external"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn what_a_tool_returned_is_recorded_against_the_invocation() {
    let kernel = kernel_with(TrustEnforcement::Observe, None, Arc::new(AllowAll::new())).await;
    let mut invocations = kernel.context().events().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = session("s1");

    run(&tools, "clean", &cx).await.unwrap();
    run(&tools, "reader", &cx).await.unwrap();

    let recorded = drain(&mut invocations);
    assert_eq!(recorded[0].output_trust, Some(Trust::Trusted));
    assert_eq!(recorded[1].output_trust, Some(Trust::Untrusted));
    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ledger_that_cannot_answer_stops_the_call() {
    // "We could not find out whether this conversation is tainted" is not "it is not".
    let registry = aik_tools::InProcessToolRegistry::new()
        .with_policy(Arc::new(AllowAll::new()))
        .with_trust_ledger(Arc::new(BrokenLedger));
    let mut registry = registry;
    registry
        .register(Declared::new("clean", Trust::Trusted, Reach::Contained))
        .unwrap();

    let error = registry
        .invoke(&ToolName::new("clean"), json!({}), &session("s1"))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
}
