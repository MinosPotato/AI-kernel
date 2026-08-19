//! The loop as a kernel component, wired to the real tool registry, the real policy engine,
//! the real approval broker and the real context store.
//!
//! The other test file drives the loop directly. This one proves the same loop works when
//! nothing is hand-assembled: components resolve each other by capability, decisions come
//! from configured rules, approvals come from a broker a frontend answers, and every one of
//! it is observable on the kernel event bus.

mod support;

use std::sync::Arc;

use aik_agent::{AGENT_ATTRIBUTE, AgentComponent, AgentLoopSettings};
use aik_api::agent::{Agent, AgentRequest, SessionId};
use aik_api::audit::{AuthorizationDecided, AuthorizationOutcome, InvocationOutcome, ToolInvoked};
use aik_api::context::{ContextAssembled, ContextBudget, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, ModelProvider, Role};
use aik_api::permission::{ApprovalSink, PolicyEngine};
use aik_api::tool::ToolName;
use aik_approval::{ApprovalBroker, ApprovalComponent};
use aik_context::ContextComponent;
use aik_core::prelude::*;
use aik_core::{ErrorKind, event::EventStream};
use aik_policy::RuleBasedPolicyEngine;
use aik_tools::{EchoTool, ToolsComponent};
use serde_json::json;
use support::{Reply, ScriptedModel, call, user};

/// Publishes a scripted model as the kernel's `dyn ModelProvider`.
struct StubModelComponent {
    model: Arc<ScriptedModel>,
}

#[async_trait]
impl Component for StubModelComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("model.stub").described("a scripted model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn ModelProvider>(self.model.clone())
    }
}

fn policy(rules: serde_json::Value) -> Arc<dyn PolicyEngine> {
    let config = Config::builder()
        .layer(json!({ "policy": { "rules": rules } }))
        .build();
    Arc::new(RuleBasedPolicyEngine::from_config(&config, "policy").expect("a valid policy"))
}

fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut events = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        events.push(envelope.payload);
    }
    events
}

fn agent_component() -> AgentComponent {
    AgentComponent::new("assistant", AgentLoopSettings::new("test-model"))
        .described("the kernel's default assistant")
        .requires("model.stub")
        .requires("tools.registry")
        .requires("context.store")
}

#[tokio::test]
async fn the_component_publishes_an_agent_that_runs_through_the_kernels_own_subsystems() {
    let model = Arc::new(ScriptedModel::new([
        Reply::calls([call(
            "c1",
            "kernel.echo",
            json!({ "text": "hello from a tool" }),
        )]),
        Reply::answer("the tool said hello"),
    ]));

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy(json!([
                    { "action": "kernel.echo", "effect": { "decision": "allow" } }
                ]))),
        )
        .component(ContextComponent::new())
        .component(agent_component())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let mut invocations = ctx.subscribe::<ToolInvoked>();
    let mut assemblies = ctx.subscribe::<ContextAssembled>();

    let agent = ctx.service::<dyn Agent>().unwrap();
    assert_eq!(agent.descriptor().id.as_str(), "assistant");

    let session = SessionId::new();
    let cx = user("alice");
    let response = agent
        .run(
            AgentRequest {
                session,
                input: vec![ContentPart::text("say hello")],
                context: json!(null),
            },
            &cx,
        )
        .await
        .unwrap();

    assert_eq!(response.session, session);
    assert_eq!(
        response.output,
        vec![ContentPart::text("the tool said hello")]
    );

    // The tool ran through the registry, and the run is joined to the caller's operation.
    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].tool, ToolName::new("kernel.echo"));
    assert_eq!(invoked[0].outcome, InvocationOutcome::Succeeded);
    assert_eq!(invoked[0].correlation, cx.correlation);

    // One window per turn, and cost is observable without any content leaving the store.
    let assembled = drain(&mut assemblies);
    assert_eq!(assembled.len(), 2);
    assert!(assembled.iter().all(|event| event.session == session));

    // The transcript is in the kernel's own store, attributed to the caller.
    let store = ctx.service::<dyn ContextStore>().unwrap();
    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    let roles: Vec<Role> = window.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_policy_rule_can_be_scoped_to_the_agent_the_call_came_from() {
    let model = Arc::new(ScriptedModel::new([
        Reply::calls([call("c1", "kernel.echo", json!({ "text": "hi" }))]),
        Reply::answer("done"),
    ]));

    // The attribute the loop stamps is trusted metadata: it is set from the agent's own
    // identity, never from a message or from `AgentRequest::context`, so a rule may rely on
    // it to distinguish one agent from another.
    let rules = json!([
        {
            "action": "kernel.echo",
            "context": { AGENT_ATTRIBUTE: "assistant" },
            "effect": { "decision": "allow" }
        },
        {
            "action": "*",
            "effect": { "decision": "deny", "reason": "not this agent" }
        }
    ]);

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy(rules)),
        )
        .component(ContextComponent::new())
        .component(agent_component())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let mut decisions = ctx.subscribe::<AuthorizationDecided>();

    ctx.service::<dyn Agent>()
        .unwrap()
        .run(
            AgentRequest {
                session: SessionId::new(),
                input: vec![ContentPart::text("go")],
                context: json!({ AGENT_ATTRIBUTE: "someone-else" }),
            },
            &user("alice"),
        )
        .await
        .unwrap();

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 1);
    assert_eq!(
        decided[0].outcome,
        AuthorizationOutcome::Allowed,
        "the caller's request context must not be able to impersonate another agent",
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_human_approving_through_the_gate_lets_the_tool_run() {
    let model = Arc::new(ScriptedModel::new([
        Reply::calls([call("c1", "kernel.echo", json!({ "text": "risky" }))]),
        Reply::answer("approved and done"),
    ]));
    let broker = Arc::new(ApprovalBroker::new());

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(ApprovalComponent::new(broker.clone()))
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy(json!([
                    {
                        "action": "kernel.echo",
                        "effect": { "decision": "require_approval", "prompt": "let the agent echo?" }
                    }
                ])))
                .with_approvals(broker.clone() as Arc<dyn ApprovalSink>),
        )
        .component(ContextComponent::new())
        .component(agent_component())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let mut decisions = ctx.subscribe::<AuthorizationDecided>();

    // A frontend: answers whatever the broker asks. Subscribed before the run starts, so
    // the notification cannot be missed.
    let mut pending = broker.gate().subscribe();
    let answering = tokio::spawn(async move {
        let question = pending.recv().await.expect("a question");
        assert!(question.prompt.contains("let the agent echo?"));
        pending
            .gate()
            .approve(&question.id)
            .expect("the question is live");
    });

    let response = ctx
        .service::<dyn Agent>()
        .unwrap()
        .run(AgentRequest::text("go"), &user("alice"))
        .await
        .unwrap();

    answering.await.unwrap();
    assert_eq!(
        response.output,
        vec![ContentPart::text("approved and done")]
    );
    assert_eq!(
        drain(&mut decisions)[0].outcome,
        AuthorizationOutcome::ApprovalGranted,
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_human_refusing_leaves_the_run_to_carry_on_without_the_tool() {
    let model = Arc::new(ScriptedModel::new([
        Reply::calls([call("c1", "kernel.echo", json!({ "text": "risky" }))]),
        Reply::answer("I was not allowed to"),
    ]));
    let broker = Arc::new(ApprovalBroker::new());

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(ApprovalComponent::new(broker.clone()))
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy(json!([
                    {
                        "action": "kernel.echo",
                        "effect": { "decision": "require_approval", "prompt": "let the agent echo?" }
                    }
                ])))
                .with_approvals(broker.clone() as Arc<dyn ApprovalSink>),
        )
        .component(ContextComponent::new())
        .component(agent_component())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let mut invocations = ctx.subscribe::<ToolInvoked>();

    let mut pending = broker.gate().subscribe();
    let answering = tokio::spawn(async move {
        let question = pending.recv().await.expect("a question");
        pending
            .gate()
            .deny(&question.id)
            .expect("the question is live");
    });

    let response = ctx
        .service::<dyn Agent>()
        .unwrap()
        .run(AgentRequest::text("go"), &user("alice"))
        .await
        .unwrap();

    answering.await.unwrap();
    assert_eq!(
        response.output,
        vec![ContentPart::text("I was not allowed to")]
    );
    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Denied
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_with_nowhere_to_keep_its_context_refuses_to_start() {
    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: Arc::new(ScriptedModel::new([Reply::answer("never")])),
        })
        .component(ToolsComponent::new())
        .component(
            AgentComponent::new("assistant", AgentLoopSettings::new("test-model"))
                .requires("model.stub")
                .requires("tools.registry"),
        )
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lifecycle);
    assert!(
        error.to_string().contains("agent.loop"),
        "the failure must name the component that could not be wired: {error}",
    );
}

#[tokio::test]
async fn several_agents_can_share_one_kernel_without_sharing_a_tool_set() {
    let model = Arc::new(ScriptedModel::new([
        Reply::calls([call("c1", "kernel.echo", json!({ "text": "hi" }))]),
        Reply::answer("done"),
    ]));

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(
            ToolsComponent::new()
                .with_tool(EchoTool::new())
                .with_policy(policy(json!([
                    { "action": "kernel.echo", "effect": { "decision": "allow" } }
                ]))),
        )
        .component(ContextComponent::new())
        .component(agent_component())
        .component(
            AgentComponent::new("restricted", AgentLoopSettings::new("test-model"))
                .with_id("agent.restricted")
                .as_default(false)
                .with_tools([ToolName::new("nothing.at.all")])
                .requires("model.stub")
                .requires("tools.registry")
                .requires("context.store"),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let restricted = ctx
        .service_named::<dyn Agent>(&ComponentId::new("agent.restricted"))
        .unwrap();
    assert_eq!(restricted.descriptor().id.as_str(), "restricted");

    let mut invocations = ctx.subscribe::<ToolInvoked>();
    restricted
        .run(AgentRequest::text("go"), &user("alice"))
        .await
        .unwrap();

    assert!(
        drain(&mut invocations).is_empty(),
        "an agent restricted to tools that do not exist can invoke nothing",
    );
    assert!(model.request(0).tools.is_empty());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_run_under_no_principal_is_the_system_acting_for_itself() {
    let model = Arc::new(ScriptedModel::new([Reply::answer("done")]));

    let kernel = Kernel::builder()
        .component(StubModelComponent {
            model: model.clone(),
        })
        .component(ToolsComponent::new())
        .component(ContextComponent::new())
        .component(agent_component())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let session = SessionId::new();
    ctx.service::<dyn Agent>()
        .unwrap()
        .run(
            AgentRequest {
                session,
                input: vec![ContentPart::text("go")],
                context: json!(null),
            },
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    let store = ctx.service::<dyn ContextStore>().unwrap();
    let stats = store
        .stats(&session, &ExecutionContext::new())
        .await
        .unwrap()
        .expect("the session exists");
    assert_eq!(stats.owner.as_str(), "system");

    // And a named principal cannot then reach into it.
    assert_eq!(
        store
            .stats(&session, &user("alice"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Permission,
    );

    kernel.shutdown().await.unwrap();
}
