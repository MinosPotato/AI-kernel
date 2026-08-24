//! A host process started in-process, around a scripted model.
//!
//! Everything except the model is the shipped path: the real wiring from
//! [`aik_runtime::wiring::builder`], the real tool registry, the real policy engine, the real
//! approval broker, the real stores, the real listener with its real file modes, and
//! [`aik_daemon::serve_assembled`] itself. Only the model is scripted, because the one thing a
//! test cannot have is a language model that reliably says what the test is about.

// A shared test module is compiled into every integration test binary, so anything one of
// them does not use looks dead, and nothing in a test binary is reachable from outside it.
#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelCapabilities, ModelDescriptor, ModelId, ModelProvider, Role,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::ComponentId;
use aik_core::prelude::*;
use aik_daemon::args::Options;
use aik_daemon::settings::DaemonSettings;
use aik_ipc::protocol::{Reply as WireReply, Request};
use aik_ipc::{Client, Connected, Endpoint};
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The component id the scripted provider registers under.
pub const STUB_MODEL: &str = "model.stub";

/// The model id every scripted turn is answered by.
pub const SCRIPTED: &str = "scripted";

/// How long a test waits for the host to be listening, or to stop.
const PATIENCE: Duration = Duration::from_secs(10);

/// One scripted turn.
#[derive(Debug, Clone)]
pub enum Turn {
    /// Ask for tools and say nothing else.
    Calls(Vec<ToolCall>),
    /// Answer, ending the run.
    Answer(String),
    /// Fail the request outright, as an unreachable server would.
    Fail(String),
}

impl Turn {
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

/// A provider that replays a script, and can be told to take its time.
#[derive(Debug)]
pub struct ScriptedModel {
    replies: Mutex<std::collections::VecDeque<Turn>>,
    requests: Mutex<Vec<CompletionRequest>>,
    /// How long each completion takes, so a test can have a turn that is still running.
    delay: Duration,
}

impl ScriptedModel {
    /// A model that will answer with `replies`, in order.
    pub fn new(replies: impl IntoIterator<Item = Turn>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        }
    }

    /// Makes every completion take `delay`, so a turn can be observed mid-flight.
    #[must_use]
    pub fn slowed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// How many completions the model has been asked for.
    pub fn completions(&self) -> usize {
        self.requests.lock().expect("not poisoned").len()
    }

    /// Every request the model was sent.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        Ok(vec![ModelDescriptor {
            id: ModelId::new(SCRIPTED),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities(vec![ModelCapabilities::TOOLS.to_owned()]),
        }])
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        self.requests.lock().expect("not poisoned").push(request);
        if !self.delay.is_zero() {
            // Cancellable, so a test can assert that cancelling a call actually reaches the
            // model call underneath it rather than merely detaching from it.
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {}
                () = cx.cancelled() => return Err(Error::Cancelled),
            }
        }
        let reply = self
            .replies
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| Turn::answer("the script ran out"));
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

/// Asking a host something, with a bound on how long it may take.
///
/// Used everywhere in place of [`Client::call`], so that a host which stops answering fails
/// the test in seconds with a message naming the request, rather than hanging the suite.
pub trait Answers {
    /// Sends one request and waits for its answer, failing the test if none arrives.
    async fn answered(&mut self, request: Request) -> aik_core::Result<WireReply>;
}

impl Answers for Client {
    async fn answered(&mut self, request: Request) -> aik_core::Result<WireReply> {
        let described = format!("{request:?}");
        match tokio::time::timeout(PATIENCE, self.call(request)).await {
            Ok(answer) => answer,
            Err(_) => panic!("the host did not answer {described} within {PATIENCE:?}"),
        }
    }
}

/// A running host, and everything a test needs to talk to it or to take it away.
pub struct Host {
    /// The socket and token file the host is serving on.
    pub endpoint: Endpoint,
    /// The resolved settings the host was built from.
    pub settings: DaemonSettings,
    /// The scripted model, for asserting on what it was sent.
    pub model: Arc<ScriptedModel>,
    /// Cancelling this asks the host to stop, exactly as a signal would.
    pub shutdown: CancellationToken,
    /// The directory holding this host's database, kept alive for its lifetime.
    ///
    /// Deliberately *not* the filesystem root: a database inside the directory the agent's
    /// tools are confined to would be a file the agent could read, and would show up in every
    /// directory listing a test asserts on.
    pub data: Option<tempfile::TempDir>,
    serving: tokio::task::JoinHandle<Result<()>>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("socket", &self.endpoint.socket())
            .finish_non_exhaustive()
    }
}

impl Host {
    /// Connects a client, as `aik --socket` would.
    pub async fn connect(&self, interactive: bool) -> Result<(Client, Connected)> {
        Client::connect(&self.endpoint, "test", interactive).await
    }

    /// Connects a client and asserts it was accepted.
    pub async fn client(&self, interactive: bool) -> Client {
        self.connect(interactive)
            .await
            .expect("the host accepts a client of this account with its own token")
            .0
    }

    /// Asks one question and returns the answer, failing the test on a refusal.
    pub async fn ask(&self, request: Request) -> WireReply {
        let mut client = self.client(false).await;
        client.answered(request).await.expect("the host answered")
    }

    /// Stops the host and waits for it, as a signal followed by an exit would.
    pub async fn stop(self) -> Result<()> {
        self.shutdown.cancel();
        let Self { serving, data, .. } = self;
        let outcome = tokio::time::timeout(PATIENCE, serving)
            .await
            .expect("the host stops within the patience of this test")
            .expect("the serving task did not panic");
        // Dropped after the host has stopped, never before: removing the directory out from
        // under an open database would fail a test for the wrong reason.
        drop(data);
        outcome
    }

    /// Stops the host and asserts it stopped cleanly.
    pub async fn shut_down(self) {
        let socket = self.endpoint.socket().to_path_buf();
        let token = self.endpoint.token().to_path_buf();
        self.stop().await.expect("the host stops cleanly");
        assert!(!socket.exists(), "a socket must not outlive its host");
        assert!(
            !token.exists(),
            "a credential must not outlive the host it authenticates",
        );
    }
}

/// Builds a host: a temporary root and socket, a policy document, and a script.
pub struct HostBuilder {
    policy: Option<Value>,
    config: Option<Value>,
    turns: Vec<Turn>,
    delay: Duration,
    options: Options,
    database: Option<PathBuf>,
    socket: Option<PathBuf>,
}

impl Default for HostBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HostBuilder {
    /// A host with read-only tools, no policy, and nothing to say.
    pub fn new() -> Self {
        Self {
            policy: None,
            config: None,
            turns: Vec::new(),
            delay: Duration::ZERO,
            options: Options::default(),
            database: None,
            socket: None,
        }
    }

    /// Starts the host with `--config <file>` holding this tree.
    ///
    /// Written to a file and passed as the flag rather than injected into the resolved
    /// settings, because the thing worth testing is the resolution: a key the host reads from
    /// somewhere a shipped configuration file does not put it is exactly the failure this
    /// exists to catch.
    #[must_use]
    pub fn config(mut self, config: Value) -> Self {
        self.config = Some(config);
        self
    }

    /// The policy document's rules.
    #[must_use]
    pub fn policy(mut self, rules: Value) -> Self {
        self.policy = Some(json!({ "rules": rules }));
        self
    }

    /// What the model says, in order.
    #[must_use]
    pub fn says(mut self, turns: impl IntoIterator<Item = Turn>) -> Self {
        self.turns = turns.into_iter().collect();
        self
    }

    /// How long each completion takes.
    #[must_use]
    pub fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Registers the filesystem write tool as well.
    #[must_use]
    pub fn writable(mut self) -> Self {
        self.options.write = true;
        self
    }

    /// Serves at most `clients` connections at once.
    #[must_use]
    pub fn max_clients(mut self, clients: usize) -> Self {
        self.options.max_connections = Some(clients);
        self
    }

    /// Keeps nothing on disk.
    #[must_use]
    pub fn ephemeral(mut self) -> Self {
        self.options.ephemeral = true;
        self
    }

    /// Reuses an existing database, which is what a restart does.
    #[must_use]
    pub fn database(mut self, path: impl Into<PathBuf>) -> Self {
        self.database = Some(path.into());
        self
    }

    /// Serves on an existing socket path, which is what a restart does.
    #[must_use]
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Resolves the settings this builder describes, without starting anything.
    ///
    /// `data` is where a database goes when the builder was not given one, and is `None` for
    /// an ephemeral host.
    pub fn settings(&self, root: &Path, data: Option<&Path>) -> DaemonSettings {
        let mut options = self.options.clone();
        options.root = Some(root.to_path_buf());
        options.model = Some(SCRIPTED.to_owned());
        options.database = self
            .database
            .clone()
            .or_else(|| data.map(|data| data.join("aik.redb")));
        options.socket = Some(
            self.socket
                .clone()
                .unwrap_or_else(|| root.join("run").join("aikd.sock")),
        );

        if let Some(config) = &self.config {
            let path = root.join(".aikd.json");
            std::fs::write(&path, config.to_string()).expect("a configuration file");
            options.config = Some(path);
        }

        if let Some(policy) = &self.policy {
            let path = root.join(".policy.json");
            std::fs::write(&path, policy.to_string()).expect("a policy file");
            options.policy = Some(path);
        }

        // An environment with no `HOME` and no XDG variables: a bug that lost the explicit
        // path fails the test rather than quietly opening the database, or the socket, of
        // whoever ran the suite.
        DaemonSettings::resolve_from(&options, Vec::<(String, String)>::new())
            .expect("resolved settings")
    }

    /// Starts the host and waits until it is listening.
    pub async fn start(self, root: &Path) -> Host {
        // Durable by default, exactly as the shipped host is — but never at the path the
        // shipped host would choose. The environment handed to the resolver is empty of
        // `HOME`, `XDG_DATA_HOME` and `XDG_RUNTIME_DIR`, so a bug that lost the explicit path
        // fails the test rather than quietly opening the database, or the socket, of whoever
        // ran the suite.
        let implicit = !self.options.ephemeral && self.database.is_none();
        let data = implicit.then(|| tempfile::tempdir().expect("a temporary data directory"));
        let mut settings = self.settings(root, data.as_ref().map(tempfile::TempDir::path));
        // The one substitution: the agent component declares a dependency on whichever
        // component publishes the model provider, by name, so the stub has to be named here
        // rather than merely registered.
        settings.runtime.model_component = ComponentId::new(STUB_MODEL);
        let model = Arc::new(ScriptedModel::new(self.turns.clone()).slowed(self.delay));

        let (builder, broker) =
            aik_runtime::wiring::builder(&settings.runtime, ModelId::new(SCRIPTED))
                .expect("the shipped wiring");
        let kernel = builder
            .component(StubModelComponent::new(model.clone()))
            .build()
            .expect("a kernel");

        let shutdown = CancellationToken::new();
        let endpoint = settings.endpoint.clone();
        let serving = tokio::spawn({
            let settings = settings.clone();
            let shutdown = shutdown.clone();
            async move {
                aik_daemon::serve_assembled(
                    &settings,
                    ModelId::new(SCRIPTED),
                    aik_runtime::wiring::Assembled { kernel, broker },
                    shutdown,
                )
                .await
            }
        });

        await_listening(endpoint.socket()).await;

        Host {
            endpoint,
            settings,
            model,
            shutdown,
            data,
            serving,
        }
    }
}

/// Waits until something is accepting connections at `socket`.
///
/// Polled rather than signalled because the property a test cares about is the one a client
/// sees: the socket is there and it answers. A host that told the test it had started before
/// the socket existed would hide exactly the race worth catching.
pub async fn await_listening(socket: &Path) {
    let deadline = std::time::Instant::now() + PATIENCE;
    while std::time::Instant::now() < deadline {
        if aik_ipc::is_listening(socket) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "nothing is listening at {} after {PATIENCE:?}",
        socket.display()
    );
}

/// Waits until nothing is accepting connections at `socket`.
pub async fn await_stopped(socket: &Path) {
    let deadline = std::time::Instant::now() + PATIENCE;
    while std::time::Instant::now() < deadline {
        if !aik_ipc::is_listening(socket) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("something is still listening at {}", socket.display());
}

/// Everything, at both levels. For tests about something other than policy.
pub fn permissive() -> Value {
    json!([
        { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "*", "effect": { "decision": "allow" } }
    ])
}

/// Reading is allowed as a capability, and every specific file is put to a human.
///
/// Two rules because a rule with no `resource` answers only the capability-level question and
/// a rule with `"*"` answers both; ordering them this way means exactly one question per call,
/// about the file, rather than one about the capability as well.
pub fn ask_per_file() -> Value {
    json!([
        { "action": "*", "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "resource": "*",
          "effect": { "decision": "require_approval", "prompt": "let it read?" } }
    ])
}
