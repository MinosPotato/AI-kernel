//! Driving one conversation.
//!
//! The frontend's whole job, and the limit of what it is allowed to be: read input, hand it
//! to [`Agent::stream`], print what comes back, and put approval questions to the person at
//! the terminal. It holds no tool, resolves no policy, and decides nothing about whether an
//! operation may happen — those live behind
//! [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke), where the agent reaches
//! them and this module cannot.
//!
//! # The execution context
//!
//! Every turn runs under a context carrying [`Settings::principal`], which is the *agent*
//! acting on behalf of the user. That is the identity policy sees, the identity audit events
//! record, and the identity that owns the transcript in the
//! [`ContextStore`](aik_api::context::ContextStore).

use std::sync::Arc;

use aik_api::agent::{Agent, AgentRequest, AgentUpdate, SessionId};
use aik_api::audit::{AuthorizationDecided, ToolInvoked};
use aik_api::context::ContextAssembled;
use aik_api::execution::ExecutionContext;
use aik_api::model::ContentPart;
use aik_api::permission::Principal;
use aik_approval::{ApprovalStream, PendingApproval};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use futures::StreamExt;
use serde_json::Value;
use tokio::io::AsyncBufRead;

use crate::approval;
use crate::console::Console;
use crate::render::{self, TurnStats};
use crate::settings::Settings;

/// The prompt shown before each line of input.
pub const PROMPT: &str = "\n› ";

/// What one iteration of the drive loop observed.
enum Step {
    /// The agent produced something.
    Update(Option<Result<AgentUpdate>>),
    /// A question needs answering.
    Question(Option<PendingApproval>),
    /// The person at the terminal interrupted.
    Interrupted,
}

/// How a conversation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The conversation finished normally.
    Finished,
    /// The person asked to stop, or input ran out.
    Quit,
}

/// One conversation with one agent.
pub struct Session<R> {
    agent: Arc<dyn Agent>,
    console: Console<R>,
    principal: Principal,
    session: SessionId,
    verbose: bool,
    /// Present only for an interactive session. Holding it is what tells the broker
    /// somebody can be asked; without it every `require_approval` is refused outright.
    approvals: Option<ApprovalStream>,
    windows: EventStream<ContextAssembled>,
    decisions: EventStream<AuthorizationDecided>,
    invocations: EventStream<ToolInvoked>,
}

impl<R> std::fmt::Debug for Session<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("session", &self.session)
            .field("principal", &self.principal)
            .field("interactive", &self.approvals.is_some())
            .finish_non_exhaustive()
    }
}

impl<R: AsyncBufRead + Unpin + Send> Session<R> {
    /// Creates a session against a started kernel.
    ///
    /// `approvals` is the difference between the two modes: `Some` for an interactive
    /// session, `None` for a one-shot run, which is what makes a one-shot run fail closed
    /// on any policy that defers to a human.
    pub fn new(
        kernel: &KernelContext,
        settings: &Settings,
        console: Console<R>,
        approvals: Option<ApprovalStream>,
    ) -> Result<Self> {
        Ok(Self {
            agent: kernel.service::<dyn Agent>()?,
            console,
            principal: settings.principal(),
            session: SessionId::new(),
            verbose: settings.verbose,
            approvals,
            windows: kernel.subscribe::<ContextAssembled>(),
            decisions: kernel.subscribe::<AuthorizationDecided>(),
            invocations: kernel.subscribe::<ToolInvoked>(),
        })
    }

    /// The conversation this session is appending to.
    pub fn id(&self) -> SessionId {
        self.session
    }

    /// Runs one prompt to completion and returns.
    pub async fn one_shot(&mut self, prompt: String) -> Result<()> {
        self.turn(prompt).await
    }

    /// Reads and answers prompts until the person stops or input runs out.
    pub async fn interactive(&mut self) -> Result<Outcome> {
        loop {
            let line = tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    println!();
                    return Ok(Outcome::Quit);
                }
                line = self.console.ask(PROMPT) => line?,
            };

            let Some(line) = line else {
                println!();
                return Ok(Outcome::Quit);
            };
            let line = line.trim();

            if line.is_empty() {
                continue;
            }
            if let Some(command) = line.strip_prefix('/') {
                match self.command(command) {
                    Outcome::Quit => return Ok(Outcome::Quit),
                    Outcome::Finished => continue,
                }
            }

            // A failed turn ends the turn, not the session: a refused tool, a model that
            // ran out of time, a window that could not be built are all things to report
            // and carry on from.
            if let Err(error) = self.turn(line.to_owned()).await {
                println!("  error: {error}");
            }
        }
    }

    /// Handles a `/command`, reporting whether the session should end.
    fn command(&mut self, command: &str) -> Outcome {
        match command.trim() {
            "quit" | "q" | "exit" => Outcome::Quit,
            "new" => {
                self.session = SessionId::new();
                println!("  started a new conversation");
                Outcome::Finished
            }
            "session" => {
                println!("  session {}", self.session);
                match &self.principal.on_behalf_of {
                    Some(user) => println!("  acting as {} for {user}", self.principal.id),
                    None => println!("  acting as {}", self.principal.id),
                }
                Outcome::Finished
            }
            "tools" => {
                // The agent's own declared set, which is all the frontend can see: it holds
                // no `ToolRegistry`, deliberately, so it cannot enumerate — or reach — the
                // tools the registry actually has.
                let descriptor = self.agent.descriptor();
                if descriptor.tools.is_empty() {
                    println!("  this agent is not restricted to a fixed set;");
                    println!("  it is offered whatever the tool registry lists each run");
                } else {
                    for tool in descriptor.tools {
                        println!("  {tool}");
                    }
                }
                Outcome::Finished
            }
            other => {
                if !other.is_empty() && other != "help" {
                    println!("  unknown command `/{other}`");
                }
                println!("  /new  /session  /tools  /quit");
                Outcome::Finished
            }
        }
    }

    /// Runs one prompt, printing updates and answering approvals as they arrive.
    async fn turn(&mut self, input: String) -> Result<()> {
        let cx = ExecutionContext::new().with_principal(self.principal.clone());
        let request = AgentRequest {
            session: self.session,
            input: vec![ContentPart::text(input)],
            context: Value::Null,
        };

        let mut updates = self.agent.stream(request, &cx).await?;
        let mut stats = TurnStats::default();
        let mut interrupted = false;

        loop {
            // Each branch only carries a value out; the work happens after the select, so
            // that answering a question can borrow the console the loop also reads from.
            let step = tokio::select! {
                biased;
                question = next_question(&mut self.approvals) => Step::Question(question),
                _ = tokio::signal::ctrl_c(), if !interrupted => Step::Interrupted,
                update = updates.next() => Step::Update(update),
            };

            match step {
                Step::Question(Some(pending)) => {
                    if let Some(stream) = &self.approvals {
                        approval::answer(stream, &pending, &mut self.console).await?;
                    }
                }
                // The broker is gone, so no further question can arrive. Dropping the
                // stream stops the branch from completing immediately forever.
                Step::Question(None) => self.approvals = None,
                Step::Interrupted => {
                    interrupted = true;
                    println!("\n  interrupting…");
                    cx.cancellation.cancel();
                }
                Step::Update(None) => break,
                Step::Update(Some(Ok(update))) => {
                    self.drain(&mut stats);
                    render::update(&update, &mut stats);
                }
                Step::Update(Some(Err(error))) => {
                    self.drain(&mut stats);
                    return Err(error);
                }
            }
        }

        self.drain(&mut stats);
        Ok(())
    }

    /// Consumes whatever the kernel published since the last check.
    ///
    /// Polled rather than selected on: these are diagnostics, and a missed one must never
    /// be able to stall the conversation. Lag is ignored for the same reason.
    fn drain(&mut self, stats: &mut TurnStats) {
        while let Some(Ok(envelope)) = self.windows.try_recv() {
            stats.record(&envelope.payload);
            if self.verbose {
                render::assembled(&envelope.payload);
            }
        }
        while let Some(Ok(envelope)) = self.decisions.try_recv() {
            if self.verbose {
                render::authorization(&envelope.payload);
            }
        }
        while let Some(Ok(envelope)) = self.invocations.try_recv() {
            if self.verbose {
                render::invocation(&envelope.payload);
            }
        }
    }
}

/// Waits for the next question, or forever when nobody is listening for them.
async fn next_question(approvals: &mut Option<ApprovalStream>) -> Option<PendingApproval> {
    match approvals {
        Some(stream) => stream.recv().await,
        // A one-shot run reaches this and stays here. The broker refuses its questions
        // without ever asking, because no gate exists to ask through.
        None => std::future::pending().await,
    }
}

/// The type the frontend actually uses; the generic parameter exists for tests.
pub type StdioSession = Session<tokio::io::BufReader<tokio::io::Stdin>>;

/// Convenience for the common case: a session reading the process's standard input.
pub fn stdio(
    kernel: &KernelContext,
    settings: &Settings,
    approvals: Option<ApprovalStream>,
) -> Result<StdioSession> {
    Session::new(kernel, settings, Console::stdio(), approvals)
}
