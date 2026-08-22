//! A whole kernel with an agent in it, for the suites about what an agent can reach.
//!
//! Everything here is real except the model. A model that answered from a network would make
//! these tests non-deterministic without making them test anything more: what is under test
//! is the path *between* a model's request and a subsystem, and that path is identical
//! whether the request came from a scripted reply or from a real completion. Every other
//! collaborator — the registry, the policy engine, the context store, the memory store, the
//! agent loop — is the one that ships.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aik_agent::{AgentComponent, AgentLoopSettings};
use aik_api::agent::{Agent, AgentRequest, AgentUpdate};
use aik_api::execution::ExecutionContext;
use aik_api::memory::MemoryStore;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelDescriptor, ModelProvider, Role,
};
use aik_api::permission::PolicyEngine;
use aik_api::tool::{ToolCall, ToolName, ToolOutcome};
use aik_context::ContextComponent;
use aik_core::clock::{ManualClock, Timestamp};
use aik_core::prelude::*;
use aik_core::{Config, Error, Result};
use aik_memory::{MemoryComponent, MemoryToolsComponent, RedbMemoryComponent};
use aik_policy::RuleBasedPolicyEngine;
use aik_store::StoreComponent;
use aik_tools::ToolsComponent;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::store_config;

/// One scripted model turn.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A final answer with no tool calls.
    Answer(String),
    /// A turn that asks for tools.
    Calls(Vec<ToolCall>),
}

impl Reply {
    /// A final answer.
    pub fn answer(text: impl Into<String>) -> Self {
        Self::Answer(text.into())
    }

    /// A turn asking for one tool.
    pub fn call(id: &str, tool: &str, arguments: Value) -> Self {
        Self::Calls(vec![ToolCall {
            call_id: id.to_owned(),
            name: ToolName::new(tool),
            arguments,
        }])
    }

    fn into_response(self) -> CompletionResponse {
        match self {
            Self::Answer(text) => CompletionResponse {
                message: Message::text(Role::Assistant, text),
                finish_reason: FinishReason::Stop,
                usage: None,
            },
            Self::Calls(calls) => CompletionResponse {
                message: Message {
                    role: Role::Assistant,
                    content: calls.into_iter().map(ContentPart::ToolCall).collect(),
                    name: None,
                },
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            },
        }
    }
}

/// A [`ModelProvider`] that answers from a queue the test fills, one turn at a time.
///
/// A queue rather than a fixed script because one kernel serves several agent runs here, and
/// what each run is *for* is clearest next to the assertions about it.
pub struct ScriptedModel {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl std::fmt::Debug for ScriptedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedModel")
            .field("pending", &self.replies.lock().expect("no panics").len())
            .finish_non_exhaustive()
    }
}

impl ScriptedModel {
    /// A model with nothing scripted yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Queues the turns of one agent run.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.replies.lock().expect("no panics").extend(replies);
    }

    /// Every completion request the loop has sent.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("no panics").clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        self.requests.lock().expect("no panics").push(request);
        let reply = self.replies.lock().expect("no panics").pop_front();
        match reply {
            Some(reply) => Ok(reply.into_response()),
            None => Err(Error::other("the script ran out of turns")),
        }
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        Err(Error::Unsupported(
            "the scripted model does not stream".to_owned(),
        ))
    }
}

/// Publishes the scripted model as the kernel's `dyn ModelProvider`.
#[derive(Debug)]
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

/// Which memory implementation a suite is running against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The volatile store.
    Memory,
    /// The durable store, over a database in a temporary directory.
    Redb,
}

/// A kernel with an agent, a tool registry, a policy engine and a memory store in it.
pub struct MemoryAgentKernel {
    kernel: Option<Kernel>,
    model: Arc<ScriptedModel>,
    clock: Arc<ManualClock>,
    backend: Backend,
    rules: Value,
    directory: Option<TempDir>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for MemoryAgentKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryAgentKernel")
            .field("backend", &self.backend)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Rules that allow every memory operation, at capability level and on every resource.
pub fn allow_all_memory() -> Value {
    json!([
        { "action": "memory.*", "resource": "*", "effect": { "decision": "allow" } }
    ])
}

/// Builds a rule-based policy engine from a rules document.
pub fn policy(rules: &Value) -> Arc<dyn PolicyEngine> {
    let config = Config::builder()
        .layer(json!({ "policy": { "rules": rules } }))
        .build();
    Arc::new(RuleBasedPolicyEngine::from_config(&config, "policy").expect("a valid policy"))
}

impl Backend {
    /// Starts a kernel wired for this backend, with `rules` as its policy.
    pub async fn open_agent(self, rules: Value) -> MemoryAgentKernel {
        let clock = Arc::new(ManualClock::new(Timestamp::EPOCH));
        let (directory, path) = match self {
            Self::Memory => (None, None),
            Self::Redb => {
                let directory = tempfile::tempdir().expect("a temporary directory");
                let path = directory.path().join("aik.redb");
                (Some(directory), Some(path))
            }
        };
        let model = ScriptedModel::new();
        let kernel = start(self, &rules, model.clone(), clock.clone(), path.as_deref()).await;
        MemoryAgentKernel {
            kernel: Some(kernel),
            model,
            clock,
            backend: self,
            rules,
            directory,
            path,
        }
    }
}

impl MemoryAgentKernel {
    /// The model the agent talks to, for scripting a run.
    pub fn model(&self) -> &Arc<ScriptedModel> {
        &self.model
    }

    /// The kernel clock.
    pub fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// A handle onto the running kernel.
    pub fn context(&self) -> KernelContext {
        self.kernel
            .as_ref()
            .expect("the kernel is running")
            .context()
    }

    /// The memory store the tools were bound to, for asserting on what was actually stored.
    ///
    /// Deliberately returns a fresh handle each time rather than caching one: a handle kept
    /// alive past a restart would keep the database open with it.
    pub fn store(&self) -> Arc<dyn MemoryStore> {
        self.context()
            .service::<dyn MemoryStore>()
            .expect("the memory store is published")
    }

    /// Runs the agent for one request, returning everything it reported.
    pub async fn run(&self, input: &str, cx: &ExecutionContext) -> Result<Vec<AgentUpdate>> {
        let agent = self.context().service::<dyn Agent>()?;
        let mut stream = agent.stream(AgentRequest::text(input), cx).await?;
        let mut updates = Vec::new();
        while let Some(update) = stream.next().await {
            updates.push(update?);
        }
        Ok(updates)
    }

    /// Every tool outcome one run produced, in order.
    pub fn outcomes(updates: &[AgentUpdate]) -> Vec<ToolOutcome> {
        updates
            .iter()
            .filter_map(|update| match update {
                AgentUpdate::ToolResult { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect()
    }

    /// The single tool outcome one run produced.
    pub fn outcome(updates: &[AgentUpdate]) -> ToolOutcome {
        let mut outcomes = Self::outcomes(updates);
        assert_eq!(outcomes.len(), 1, "expected exactly one tool call");
        outcomes.remove(0)
    }

    /// Stops the kernel and starts a new one over the same database, as a restart would.
    pub async fn restart(&mut self) {
        let kernel = self.kernel.take().expect("the kernel is running");
        kernel.shutdown().await.expect("the kernel stops cleanly");
        // The database is held by the registry the kernel owns and by the binding behind
        // every memory tool it registered; redb refuses to open one file twice, so the old
        // kernel has to be gone before the new one opens the same path.
        drop(kernel);
        self.kernel = Some(
            start(
                self.backend,
                &self.rules,
                self.model.clone(),
                self.clock.clone(),
                self.path.as_deref(),
            )
            .await,
        );
    }

    /// Stops the kernel, failing the test if it does not stop cleanly.
    pub async fn shutdown(mut self) {
        if let Some(kernel) = self.kernel.take() {
            kernel.shutdown().await.expect("the kernel stops cleanly");
        }
        drop(self.directory.take());
    }
}

/// Assembles and starts one kernel: model, memory, memory tools, registry, context, agent.
async fn start(
    backend: Backend,
    rules: &Value,
    model: Arc<ScriptedModel>,
    clock: Arc<ManualClock>,
    path: Option<&std::path::Path>,
) -> Kernel {
    let memory_tools = MemoryToolsComponent::new();
    let tools = ToolsComponent::new()
        .with_tool(memory_tools.put())
        .with_tool(memory_tools.get())
        .with_tool(memory_tools.query())
        .with_tool(memory_tools.delete())
        .with_policy(policy(rules));

    let builder = Kernel::builder().clock(clock);
    let builder = match backend {
        Backend::Memory => builder.component(MemoryComponent::new()),
        Backend::Redb => builder
            .config(store_config(path.expect("a durable backend has a path")))
            .component(StoreComponent::new())
            .component(RedbMemoryComponent::new()),
    };

    let kernel = builder
        .component(memory_tools)
        .component(StubModelComponent { model })
        .component(tools)
        .component(ContextComponent::new())
        .component(
            AgentComponent::new("assistant", AgentLoopSettings::new("test-model"))
                .requires("model.stub")
                .requires("tools.registry")
                .requires("context.store"),
        )
        .build()
        .expect("a valid kernel");
    kernel.start().await.expect("the kernel starts");
    kernel
}
