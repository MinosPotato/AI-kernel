//! Scriptable stand-ins for the collaborators an agent loop is wired to.
//!
//! Nothing here is a mock in the "verify these calls happened" sense. Each piece is a real
//! implementation of a real contract that happens to be observable: a model that answers
//! from a script and keeps the requests it was sent, a tool that records the execution
//! context it ran under, a policy engine that records the questions it was asked. That is
//! what makes it possible to assert on *what the loop did*, rather than on what it says it
//! did.

// A shared test module is compiled into every integration test binary, so anything one of
// them does not use looks dead, and nothing in a test binary is reachable from outside it.
#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_agent::{AgentLoop, AgentLoopSettings};
use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextRecord, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelDescriptor, ModelProvider, Role, Usage,
};
use aik_api::permission::{
    ActionId, ApprovalSink, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind,
    ResourceAuthorizer,
};
use aik_api::tool::{ResourceClaim, Tool, ToolCall, ToolName, ToolOutcome, ToolSpec};
use aik_context::InMemoryContextStore;
use aik_core::clock::{ManualClock, SharedClock, SystemClock, Timestamp};
use aik_core::event::EventBus;
use aik_core::id::{ComponentId, CorrelationId};
use aik_core::{Error, Result};
use aik_tools::InProcessToolRegistry;
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde_json::{Map, Value, json};

// --- the model ---------------------------------------------------------------------

/// One scripted answer.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A response the provider returns.
    Response(CompletionResponse),
    /// The provider could not answer at all.
    Failure(String),
}

impl Reply {
    /// A final answer with no tool calls.
    pub fn answer(text: impl Into<String>) -> Self {
        Self::Response(CompletionResponse {
            message: Message::text(Role::Assistant, text),
            finish_reason: FinishReason::Stop,
            usage: None,
        })
    }

    /// A turn that asks for tools and says nothing else.
    pub fn calls(calls: impl IntoIterator<Item = ToolCall>) -> Self {
        Self::Response(CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content: calls.into_iter().map(ContentPart::ToolCall).collect(),
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        })
    }

    /// A turn that both says something and asks for a tool.
    pub fn saying(text: impl Into<String>, calls: impl IntoIterator<Item = ToolCall>) -> Self {
        let mut content = vec![ContentPart::text(text)];
        content.extend(calls.into_iter().map(ContentPart::ToolCall));
        Self::Response(CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        })
    }

    /// A response the provider itself reports as cancelled.
    pub fn cancelled() -> Self {
        Self::Response(CompletionResponse {
            message: Message::text(Role::Assistant, "partial"),
            finish_reason: FinishReason::Cancelled,
            usage: None,
        })
    }

    /// A provider-level failure.
    pub fn failure(reason: impl Into<String>) -> Self {
        Self::Failure(reason.into())
    }

    /// Attaches reported token usage.
    #[must_use]
    pub fn costing(self, input_tokens: u64, output_tokens: u64) -> Self {
        match self {
            Self::Response(mut response) => {
                response.usage = Some(Usage {
                    input_tokens,
                    output_tokens,
                });
                Self::Response(response)
            }
            other => other,
        }
    }
}

/// Something to run at the start of a completion, given the zero-based call number.
type CallHook = Box<dyn Fn(usize) + Send + Sync>;

/// A [`ModelProvider`] that answers from a fixed script and keeps what it was asked.
pub struct ScriptedModel {
    replies: Vec<Reply>,
    repeating: bool,
    calls: AtomicUsize,
    requests: Mutex<Vec<CompletionRequest>>,
    on_call: Mutex<Option<CallHook>>,
}

impl std::fmt::Debug for ScriptedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedModel")
            .field("replies", &self.replies.len())
            .field("calls", &self.call_count())
            .finish_non_exhaustive()
    }
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: replies.into_iter().collect(),
            repeating: false,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            on_call: Mutex::new(None),
        }
    }

    /// Repeats the last scripted reply forever, for testing what bounds a runaway loop.
    #[must_use]
    pub fn repeating(mut self) -> Self {
        self.repeating = true;
        self
    }

    /// Runs `hook` at the start of every completion, with the zero-based call number.
    #[must_use]
    pub fn on_call(self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        *self.on_call.lock().expect("hook lock") = Some(Box::new(hook));
        self
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }

    /// The request sent for turn `index`, zero-based.
    pub fn request(&self, index: usize) -> CompletionRequest {
        self.requests()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no model request for turn {index}"))
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.on_call.lock().expect("hook lock").as_ref() {
            hook(index);
        }
        self.requests.lock().expect("request lock").push(request);

        let reply = match self.replies.get(index) {
            Some(reply) => reply.clone(),
            None if self.repeating && !self.replies.is_empty() => {
                self.replies[self.replies.len() - 1].clone()
            }
            None => return Err(Error::other(format!("the script has no turn {index}"))),
        };

        match reply {
            Reply::Response(response) => Ok(response),
            Reply::Failure(reason) => Err(Error::other(reason)),
        }
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        Err(Error::Unsupported(
            "the scripted model does not stream".into(),
        ))
    }
}

/// A tool call the way a model emits one.
pub fn call(id: &str, tool: &str, arguments: Value) -> ToolCall {
    ToolCall {
        call_id: id.to_owned(),
        name: ToolName::new(tool),
        arguments,
    }
}

// --- the tools ---------------------------------------------------------------------

/// What a [`ProbeTool`] does when it runs.
#[derive(Clone)]
pub enum Behaviour {
    /// Succeeds, echoing its arguments.
    Echo,
    /// Succeeds with a payload of the given size, for testing budgets.
    Bulk(usize),
    /// Cannot run at all.
    Fail(String),
    /// Runs, and reports a failure the model is meant to see.
    ReportError(String),
    /// Cancels a token, standing in for anything that stops the run mid-flight.
    Cancel(tokio_util::sync::CancellationToken),
    /// Moves a clock forward, standing in for work that consumes the run's deadline.
    Advance(Arc<ManualClock>, Duration),
    /// Declares a resource claim, so resource-level authorization has something to decide.
    Claiming(ActionId, String),
    /// Must never run.
    Forbidden,
}

impl std::fmt::Debug for Behaviour {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Echo => "Echo",
            Self::Bulk(_) => "Bulk",
            Self::Fail(_) => "Fail",
            Self::ReportError(_) => "ReportError",
            Self::Cancel(_) => "Cancel",
            Self::Advance(..) => "Advance",
            Self::Claiming(..) => "Claiming",
            Self::Forbidden => "Forbidden",
        })
    }
}

/// What a tool saw when it was invoked.
#[derive(Debug, Clone)]
pub struct Probe {
    pub arguments: Value,
    pub correlation: CorrelationId,
    pub principal: Option<Principal>,
    pub deadline: Option<Timestamp>,
    pub attributes: Map<String, Value>,
}

/// A [`Tool`] that records the execution context it ran under.
#[derive(Debug)]
pub struct ProbeTool {
    name: ToolName,
    permissions: Vec<ActionId>,
    behaviour: Behaviour,
    seen: Arc<Mutex<Vec<Probe>>>,
}

impl ProbeTool {
    pub fn new(name: &str, behaviour: Behaviour) -> Self {
        Self {
            name: ToolName::new(name),
            permissions: vec![ActionId::new(name)],
            behaviour,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Overrides the permissions the tool requires; an empty list requires none.
    #[must_use]
    pub fn requiring(mut self, permissions: impl IntoIterator<Item = ActionId>) -> Self {
        self.permissions = permissions.into_iter().collect();
        self
    }

    /// A handle onto what this tool has seen, usable after the tool is registered.
    pub fn observations(&self) -> Arc<Mutex<Vec<Probe>>> {
        self.seen.clone()
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "a test probe".to_owned(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            required_permissions: self.permissions.clone(),
            read_only: true,
        }
    }

    fn planned_resources(&self, _arguments: &Value) -> Result<Vec<ResourceClaim>> {
        match &self.behaviour {
            Behaviour::Claiming(action, resource) => {
                Ok(vec![ResourceClaim::new(action.clone(), resource.as_str())])
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        if matches!(self.behaviour, Behaviour::Forbidden) {
            panic!("`{}` must never be invoked", self.name);
        }

        self.seen.lock().expect("probe lock").push(Probe {
            arguments: arguments.clone(),
            correlation: cx.correlation,
            principal: cx.principal.clone(),
            deadline: cx.deadline,
            attributes: cx.attributes.clone(),
        });

        match &self.behaviour {
            Behaviour::Echo | Behaviour::Claiming(..) => {
                Ok(ToolOutcome::ok(json!({ "echo": arguments })))
            }
            Behaviour::Bulk(size) => Ok(ToolOutcome::ok(json!({ "body": "x".repeat(*size) }))),
            Behaviour::Fail(reason) => Err(Error::other(reason.clone())),
            Behaviour::ReportError(reason) => {
                Ok(ToolOutcome::error(json!({ "reason": reason.clone() })))
            }
            Behaviour::Cancel(token) => {
                token.cancel();
                Ok(ToolOutcome::ok(json!({ "cancelled": true })))
            }
            Behaviour::Advance(clock, duration) => {
                clock.advance(*duration);
                Ok(ToolOutcome::ok(json!({ "advanced": true })))
            }
            Behaviour::Forbidden => unreachable!("checked above"),
        }
    }
}

// --- the policy --------------------------------------------------------------------

/// A [`PolicyEngine`] with a per-action verdict, that records every question.
pub struct RecordingPolicy {
    verdicts: HashMap<ActionId, Decision>,
    fallback: Decision,
    asked: Mutex<Vec<PermissionRequest>>,
}

impl std::fmt::Debug for RecordingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingPolicy")
            .field("verdicts", &self.verdicts.len())
            .field("asked", &self.asked.lock().expect("policy lock").len())
            .finish()
    }
}

impl RecordingPolicy {
    /// Allows everything unless told otherwise.
    pub fn allowing() -> Self {
        Self {
            verdicts: HashMap::new(),
            fallback: Decision::Allow,
            asked: Mutex::new(Vec::new()),
        }
    }

    /// Denies everything unless told otherwise.
    pub fn denying(reason: &str) -> Self {
        Self {
            verdicts: HashMap::new(),
            fallback: Decision::deny(reason),
            asked: Mutex::new(Vec::new()),
        }
    }

    /// Sets the verdict for one action.
    #[must_use]
    pub fn deciding(mut self, action: &str, decision: Decision) -> Self {
        self.verdicts.insert(ActionId::new(action), decision);
        self
    }

    pub fn questions(&self) -> Vec<PermissionRequest> {
        self.asked.lock().expect("policy lock").clone()
    }
}

#[async_trait]
impl PolicyEngine for RecordingPolicy {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _cx: &ExecutionContext,
    ) -> Result<Decision> {
        self.asked
            .lock()
            .expect("policy lock")
            .push(request.clone());
        Ok(self
            .verdicts
            .get(&request.action)
            .cloned()
            .unwrap_or_else(|| self.fallback.clone()))
    }
}

/// An [`ApprovalSink`] that answers immediately, without a human.
#[derive(Debug)]
pub struct FixedApprovals {
    granted: bool,
    asked: AtomicUsize,
}

impl FixedApprovals {
    pub fn granting() -> Self {
        Self {
            granted: true,
            asked: AtomicUsize::new(0),
        }
    }

    pub fn refusing() -> Self {
        Self {
            granted: false,
            asked: AtomicUsize::new(0),
        }
    }

    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ApprovalSink for FixedApprovals {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(self.granted)
    }
}

// --- assembly ----------------------------------------------------------------------

/// An agent loop wired to observable collaborators.
pub struct Harness {
    pub model: Arc<ScriptedModel>,
    pub policy: Arc<RecordingPolicy>,
    pub store: Arc<InMemoryContextStore>,
    pub events: EventBus,
    pub agent: AgentLoop,
    pub session: SessionId,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("session", &self.session)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

/// Assembles a [`Harness`].
pub struct HarnessBuilder {
    model: ScriptedModel,
    tools: Vec<Arc<dyn Tool>>,
    policy: RecordingPolicy,
    approvals: Option<Arc<dyn ApprovalSink>>,
    settings: AgentLoopSettings,
    allowed: Option<Vec<ToolName>>,
    clock: SharedClock,
}

impl std::fmt::Debug for HarnessBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessBuilder")
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl HarnessBuilder {
    #[must_use]
    pub fn tool(mut self, tool: impl Tool) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    #[must_use]
    pub fn shared_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    #[must_use]
    pub fn policy(mut self, policy: RecordingPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn approvals(mut self, approvals: Arc<dyn ApprovalSink>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    #[must_use]
    pub fn settings(mut self, settings: AgentLoopSettings) -> Self {
        self.settings = settings;
        self
    }

    #[must_use]
    pub fn restricted_to(mut self, tools: impl IntoIterator<Item = ToolName>) -> Self {
        self.allowed = Some(tools.into_iter().collect());
        self
    }

    #[must_use]
    pub fn clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn build(self) -> Harness {
        let events = EventBus::new(1_024, self.clock.clone());
        let policy = Arc::new(self.policy);
        let model = Arc::new(self.model);

        let store = Arc::new(
            InMemoryContextStore::new()
                .with_clock(self.clock.clone())
                .with_events(events.clone(), ComponentId::new("context.store")),
        );

        let mut registry = InProcessToolRegistry::new()
            .with_policy(policy.clone())
            .with_audit(events.clone(), ComponentId::new("tools.registry"))
            .with_clock(self.clock.clone());
        if let Some(approvals) = self.approvals {
            registry = registry.with_approvals(approvals);
        }
        for tool in self.tools {
            registry.register_arc(tool).expect("unique tool names");
        }

        let mut agent = AgentLoop::new(
            "test.agent",
            model.clone(),
            Arc::new(registry),
            store.clone(),
            self.settings,
        )
        .with_clock(self.clock.clone());
        if let Some(allowed) = self.allowed {
            agent = agent.with_tools(allowed);
        }

        Harness {
            model,
            policy,
            store,
            events,
            agent,
            session: SessionId::new(),
        }
    }
}

impl Harness {
    pub fn builder(model: ScriptedModel) -> HarnessBuilder {
        HarnessBuilder {
            model,
            tools: Vec::new(),
            policy: RecordingPolicy::allowing(),
            approvals: None,
            settings: AgentLoopSettings::new("test-model"),
            allowed: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// Every record in the harness's session, oldest first.
    pub async fn transcript(&self, cx: &ExecutionContext) -> Vec<ContextRecord> {
        let window = self
            .store
            .window(&self.session, &ContextBudget::UNLIMITED, cx)
            .await
            .expect("the session is readable");
        let mut records = Vec::with_capacity(window.records.len());
        for id in window.records {
            records.push(
                self.store
                    .get(&self.session, &id, cx)
                    .await
                    .expect("the session is readable")
                    .expect("a record the window named"),
            );
        }
        records
    }
}

/// An execution context for a named user principal.
pub fn user(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
}

/// The text of a message, for assertions.
pub fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}
