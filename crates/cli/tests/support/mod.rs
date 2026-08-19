//! A model that says what a test tells it to, and the scaffolding to start the frontend's
//! real wiring around it.
//!
//! Everything else in these tests is the production path: the real tool registry, the real
//! policy engine, the real approval broker, the real context store, the real agent loop, and
//! [`aik_cli::wiring::builder`] itself. Only the model is scripted, because the one thing a
//! test cannot have is a language model that reliably calls the tool the test is about.

// A shared test module is compiled into every integration test binary, so anything one of
// them does not use looks dead, and nothing in a test binary is reachable from outside it.
#![allow(dead_code, unreachable_pub)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelCapabilities, ModelDescriptor, ModelId, ModelProvider, Role,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_approval::ApprovalBroker;
use aik_cli::args::{Options, ToolSet};
use aik_cli::settings::Settings;
use aik_core::ComponentId;
use aik_core::prelude::*;
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde_json::{Value, json};

/// The component id the scripted provider registers under.
pub const STUB_MODEL: &str = "model.stub";

/// One scripted turn.
#[derive(Debug, Clone)]
pub enum Reply {
    /// Ask for tools and say nothing else.
    Calls(Vec<ToolCall>),
    /// Answer, ending the run.
    Answer(String),
    /// Fail the request outright, as an unreachable server would.
    Fail(String),
}

impl Reply {
    /// A turn asking for one tool.
    pub fn call(id: &str, tool: &str, arguments: Value) -> Self {
        Self::Calls(vec![ToolCall {
            call_id: id.to_owned(),
            name: ToolName::new(tool),
            arguments,
        }])
    }

    /// A turn that answers.
    pub fn answer(text: &str) -> Self {
        Self::Answer(text.to_owned())
    }

    /// A turn the provider cannot complete at all.
    pub fn fail(message: &str) -> Self {
        Self::Fail(message.to_owned())
    }

    fn into_response(self) -> Result<CompletionResponse> {
        Ok(match self {
            Self::Fail(message) => return Err(Error::other(message)),
            Self::Calls(calls) => CompletionResponse {
                message: Message {
                    role: Role::Assistant,
                    content: calls.into_iter().map(ContentPart::ToolCall).collect(),
                    name: None,
                },
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            },
            Self::Answer(text) => CompletionResponse {
                message: Message::text(Role::Assistant, text),
                finish_reason: FinishReason::Stop,
                usage: None,
            },
        })
    }
}

/// A provider that replays a fixed script and records what it was sent.
#[derive(Debug)]
pub struct ScriptedModel {
    replies: Mutex<std::collections::VecDeque<Reply>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedModel {
    /// A model that will answer with `replies`, in order.
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request the model was sent.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("not poisoned").clone()
    }

    /// The tool names offered on request `index`.
    pub fn offered(&self, index: usize) -> Vec<String> {
        self.requests()[index]
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        Ok(vec![ModelDescriptor {
            id: ModelId::new("scripted"),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities(vec![ModelCapabilities::TOOLS.to_owned()]),
        }])
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        self.requests.lock().expect("not poisoned").push(request);
        let reply = self
            .replies
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| Reply::answer("the script ran out"));
        reply.into_response()
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

/// Publishes a [`ScriptedModel`] as the kernel's default `dyn ModelProvider`.
#[derive(Debug)]
pub struct StubModelComponent {
    model: Arc<ScriptedModel>,
}

impl StubModelComponent {
    /// Wraps a model as a component.
    pub fn new(model: Arc<ScriptedModel>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Component for StubModelComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(STUB_MODEL).described("a scripted model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn ModelProvider>(self.model.clone())
    }
}

/// A started kernel wired exactly as the frontend wires it, around a scripted model.
pub struct Harness {
    /// The kernel, already started.
    pub kernel: Kernel,
    /// The broker the tool registry sends approvals to.
    pub broker: Arc<ApprovalBroker>,
    /// The scripted model, for asserting on what it was sent.
    pub model: Arc<ScriptedModel>,
    /// The settings the kernel was built from.
    pub settings: Settings,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Builds a harness: a temporary root, a policy document, a tool set, and a script.
pub struct HarnessBuilder {
    policy: Option<Value>,
    tools: ToolSet,
    prompt: Option<String>,
    replies: Vec<Reply>,
    user: String,
    agent: String,
}

impl HarnessBuilder {
    /// A builder with no policy, read-only tools, and an empty script.
    pub fn new() -> Self {
        Self {
            policy: None,
            tools: ToolSet::ReadOnly,
            prompt: None,
            replies: Vec::new(),
            user: "alice".to_owned(),
            agent: "assistant".to_owned(),
        }
    }

    /// Sets the policy document's rules.
    #[must_use]
    pub fn policy(mut self, rules: Value) -> Self {
        self.policy = Some(json!({ "rules": rules }));
        self
    }

    /// Sets which filesystem tools are registered.
    #[must_use]
    pub fn tools(mut self, tools: ToolSet) -> Self {
        self.tools = tools;
        self
    }

    /// Makes this a one-shot run, which attaches no approval responder.
    #[must_use]
    pub fn one_shot(mut self, prompt: &str) -> Self {
        self.prompt = Some(prompt.to_owned());
        self
    }

    /// Names the user the agent acts for.
    #[must_use]
    pub fn user(mut self, user: &str) -> Self {
        self.user = user.to_owned();
        self
    }

    /// Appends a scripted turn.
    #[must_use]
    pub fn reply(mut self, reply: Reply) -> Self {
        self.replies.push(reply);
        self
    }

    /// Builds and starts the kernel, rooted at `root`.
    pub async fn build(self, root: &Path) -> Harness {
        let mut options = Options {
            root: Some(root.to_path_buf()),
            user: Some(self.user.clone()),
            agent: Some(self.agent.clone()),
            prompt: self.prompt.clone(),
            write: self.tools == ToolSet::ReadWrite,
            no_tools: self.tools == ToolSet::None,
            ..Options::default()
        };
        options.model = Some("scripted".to_owned());

        let policy_file = self.policy.as_ref().map(|document| {
            let path = root.join(".policy.json");
            std::fs::write(&path, document.to_string()).expect("a policy file");
            path
        });
        options.policy = policy_file;

        let mut settings = Settings::resolve_from(&options, Vec::<(String, String)>::new())
            .expect("resolved settings");
        settings.model_component = ComponentId::new(STUB_MODEL);

        let model = Arc::new(ScriptedModel::new(self.replies));
        let (builder, broker) =
            aik_cli::wiring::builder(&settings, ModelId::new("scripted")).expect("wiring");
        let kernel = builder
            .component(StubModelComponent::new(model.clone()))
            .build()
            .expect("a kernel");
        kernel.start().await.expect("started");

        Harness {
            kernel,
            broker,
            model,
            settings,
        }
    }
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self::new()
    }
}
