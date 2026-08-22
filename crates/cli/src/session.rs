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
//!
//! # Session lifecycle, and where the decisions are not
//!
//! This module can start a session, resume one, list them, clear one and compact one. It
//! decides none of those. Every command below is a call into [`ContextStore`] under the run's
//! own context, and the answer — including a refusal — is printed as it comes back.
//!
//! That is the rule the whole crate is built on, applied to the one subsystem the frontend
//! now touches directly. Concretely:
//!
//! * **The owner comes from the context, never from what was typed.** `--session` supplies an
//!   id and nothing else. The principal is built once from resolved settings, and no command
//!   here takes one.
//! * **`/sessions` shows what the store returns.** There is no filtering in this file,
//!   because a second filter would be a second place for the rule to be wrong. The store
//!   filters to what the run may act for; the frontend renders the result.
//! * **A refusal is propagated, not interpreted.** Resuming someone else's session prints the
//!   store's `PermissionDenied` and exits. The frontend never decides that a session is
//!   unreachable, and never decides that it is reachable either.
//! * **A missing session is an error, not a fresh one.** Somebody who asked to continue a
//!   particular conversation has not agreed to start a different one, and a frontend that
//!   quietly substituted an empty session would hide both a typo and a deletion.

use std::sync::Arc;

use aik_api::agent::{Agent, AgentRequest, AgentUpdate, SessionId};
use aik_api::audit::{AuthorizationDecided, ToolInvoked};
use aik_api::context::{ContextAssembled, ContextStats, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::measurement::RequestMeasured;
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
use crate::recorder::Recorder;
use crate::render::{self, SessionStats, TurnStats};
use crate::settings::Settings;

/// The prompt shown before each line of input.
pub const PROMPT: &str = "\n› ";

/// How many evictable records `/compact` keeps when no count is given.
///
/// Enough that the conversation a person is having is still there afterwards, and small
/// enough that compacting a session at the store's record bound reclaims almost all of it.
/// Pinned records — the system prompt, durable task framing — are never counted against this
/// and never removed, so the floor is really "this many turns of conversation, plus whatever
/// was pinned".
pub const DEFAULT_COMPACT_KEEP: usize = 100;

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
    measurements: EventStream<RequestMeasured>,
    /// The transcript store, for the lifecycle commands.
    ///
    /// Resolvable from the open registry by design — a context store is infrastructure, not a
    /// gated capability, and what keeps it safe is that sessions are owned rather than that it
    /// is unreachable. Holding it lets the frontend *ask*; it does not let it decide.
    context: Arc<dyn ContextStore>,
    /// What every prompt answered in this session has cost so far. See
    /// [`SessionStats`].
    session_stats: SessionStats,
    /// Where a JSONL measurement record is appended, if [`Session::with_recorder`] was
    /// called. `None` records nothing and changes nothing else about the session.
    recorder: Option<Recorder>,
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
            context: kernel.service::<dyn ContextStore>()?,
            console,
            principal: settings.principal(),
            // A run that named no session gets a fresh id, which is `SessionId`'s default.
            session: settings.session.unwrap_or_default(),
            verbose: settings.verbose,
            approvals,
            windows: kernel.subscribe::<ContextAssembled>(),
            decisions: kernel.subscribe::<AuthorizationDecided>(),
            invocations: kernel.subscribe::<ToolInvoked>(),
            measurements: kernel.subscribe::<RequestMeasured>(),
            session_stats: SessionStats::default(),
            recorder: None,
        })
    }

    /// Records every measurement event this session observes to a JSONL file.
    ///
    /// Purely observational, like the events themselves: it changes what is written to
    /// disk, never what the session does. See [`crate::recorder`] for the format and for
    /// exactly what is and is not persisted.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Recorder) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// What this session has cost so far, across every prompt answered in it.
    pub fn session_stats(&self) -> SessionStats {
        self.session_stats
    }

    /// The context every operation this session performs runs under.
    ///
    /// One constructor, used by the turn and by every lifecycle command alike, so that "who
    /// is asking" is decided in exactly one place. Two of these would be two chances for a
    /// command to run as somebody the run is not — and the difference would be invisible
    /// until it showed somebody another principal's session.
    fn cx(&self) -> ExecutionContext {
        ExecutionContext::new().with_principal(self.principal.clone())
    }

    /// The conversation this session is appending to.
    pub fn id(&self) -> SessionId {
        self.session
    }

    /// Confirms the session named by `--session` exists and this run may use it.
    ///
    /// Called before the first turn, and it is a *report* rather than a check: the store is
    /// asked for the session's statistics under this run's own context, and whatever comes
    /// back is what happens. `PermissionDenied` propagates unchanged, because the store owns
    /// that decision and the frontend must not restate it; `Ok(None)` — no such session —
    /// becomes an error rather than a fresh session, because starting a different
    /// conversation is not what was asked for.
    ///
    /// A run that was not given `--session` has nothing to resume and returns immediately.
    /// Its own new id is not looked up: it cannot exist yet, and asking would only invent a
    /// way for a randomly-collided id to be reported as somebody else's.
    pub async fn resume(&mut self, settings: &Settings) -> Result<()> {
        let Some(session) = settings.session else {
            return Ok(());
        };
        let cx = self.cx();
        let Some(stats) = self.context.stats(&session, &cx).await? else {
            return Err(Error::InvalidArgument(format!(
                "there is no session `{session}` to resume"
            )));
        };
        println!(
            "  resumed session {session}: {} record(s), ~{} tokens",
            stats.records, stats.tokens
        );
        Ok(())
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
                match self.command(command).await {
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
    ///
    /// A failed lifecycle command reports the store's error and returns to the prompt, the
    /// same way a failed turn does: a refused compaction is something to say out loud, not a
    /// reason to end the conversation.
    async fn command(&mut self, command: &str) -> Outcome {
        let (verb, argument) = match command.trim().split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (command.trim(), ""),
        };
        match verb {
            "quit" | "q" | "exit" => Outcome::Quit,
            "new" => {
                self.session = SessionId::new();
                println!("  started a new conversation");
                Outcome::Finished
            }
            "sessions" => {
                if let Err(error) = self.list_sessions().await {
                    println!("  error: {error}");
                }
                Outcome::Finished
            }
            "clear" => {
                if let Err(error) = self.clear_session().await {
                    println!("  error: {error}");
                }
                Outcome::Finished
            }
            "compact" => {
                if let Err(error) = self.compact_session(argument).await {
                    println!("  error: {error}");
                }
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
                println!("  /new  /session  /sessions  /clear  /compact [N]  /tools  /quit");
                Outcome::Finished
            }
        }
    }

    /// Prints the sessions this run may act for, and returns them.
    ///
    /// Metadata only, and that is a property of what the store hands back rather than of what
    /// is printed here: enumeration returns [`ContextStats`], which have nowhere to put a
    /// message. Nothing in this method filters, either — the store already returned exactly
    /// the sessions this principal may act for, and a second filter would be a second place
    /// for the rule to be wrong.
    ///
    /// It returns the rows rather than only printing them, and is public for the same reason:
    /// what this command *shows* is the security-relevant part of it, and a test that could
    /// only observe the store would pass just as happily if the command asked the store the
    /// wrong question. It takes no principal and grants no access — the caller gets exactly
    /// what the terminal would have shown.
    pub async fn list_sessions(&self) -> Result<Vec<ContextStats>> {
        let sessions = self.context.sessions(&self.cx()).await?;
        if sessions.is_empty() {
            println!("  no sessions");
            return Ok(sessions);
        }
        for stats in &sessions {
            let marker = if stats.session == self.session {
                "*"
            } else {
                " "
            };
            println!(
                "{marker} {}  {:>5} record(s)  ~{:>7} tokens  owner {}",
                stats.session, stats.records, stats.tokens, stats.owner
            );
        }
        println!("  {} session(s); * is the current one", sessions.len());
        Ok(sessions)
    }

    /// Discards the current session's transcript.
    ///
    /// Deliberately not followed by starting a new one. `/clear` and `/new` are different
    /// requests — one destroys a transcript, the other walks away from it — and a command
    /// that quietly did both would make the destructive half unavoidable for anyone who
    /// wanted the other.
    async fn clear_session(&mut self) -> Result<()> {
        let cx = self.cx();
        let removed = self.context.clear(&self.session, &cx).await?;
        println!(
            "  cleared session {}: {removed} record(s) removed",
            self.session
        );
        Ok(())
    }

    /// Reclaims the oldest evictable records of the current session.
    ///
    /// Deterministic and model-free: this is [`ContextStore::compact`], which drops the
    /// oldest unpinned records and keeps the newest `keep` of them. No model is called, so
    /// `/compact` costs nothing, cannot fail halfway, and produces the same result every time
    /// — and the system prompt, being pinned, is never what it removes.
    async fn compact_session(&mut self, argument: &str) -> Result<()> {
        let keep = match argument {
            "" => DEFAULT_COMPACT_KEEP,
            raw => raw.parse::<usize>().map_err(|_| {
                Error::InvalidArgument(format!(
                    "`/compact` takes the number of records to keep; `{raw}` is not one"
                ))
            })?,
        };
        let cx = self.cx();
        let before = self.context.stats(&self.session, &cx).await?;
        let removed = self.context.compact(&self.session, keep, &cx).await?;
        let after = self.context.stats(&self.session, &cx).await?;

        let reclaimed = match (&before, &after) {
            (Some(before), Some(after)) => before.tokens.saturating_sub(after.tokens),
            _ => 0,
        };
        println!(
            "  compacted session {}: {removed} record(s) removed, ~{reclaimed} tokens reclaimed, \
             {} kept",
            self.session,
            after.map_or(0, |stats| stats.records)
        );
        Ok(())
    }

    /// Runs one prompt, printing updates and answering approvals as they arrive.
    async fn turn(&mut self, input: String) -> Result<()> {
        let cx = self.cx();
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
        if self.verbose {
            render::session_totals(&self.session_stats);
        }
        Ok(())
    }

    /// Consumes whatever the kernel published since the last check.
    ///
    /// Polled rather than selected on: these are diagnostics, and a missed one must never
    /// be able to stall the conversation. Lag is ignored for the same reason — a recording
    /// or a verbose line skipped because a channel overflowed is a smaller problem than the
    /// conversation stalling to avoid it, and is exactly the trade-off
    /// [`EventBus`](aik_core::event::EventBus) itself documents.
    fn drain(&mut self, stats: &mut TurnStats) {
        while let Some(Ok(envelope)) = self.windows.try_recv() {
            stats.record(&envelope.payload);
            if self.verbose {
                render::assembled(&envelope.payload);
            }
            if let Some(recorder) = &mut self.recorder {
                recorder.record_context(&envelope.payload);
            }
        }
        while let Some(Ok(envelope)) = self.decisions.try_recv() {
            self.session_stats.record_authorization(&envelope.payload);
            if self.verbose {
                render::authorization(&envelope.payload);
            }
            if let Some(recorder) = &mut self.recorder {
                recorder.record_authorization(&envelope.payload);
            }
        }
        while let Some(Ok(envelope)) = self.invocations.try_recv() {
            self.session_stats.record_invocation(&envelope.payload);
            if self.verbose {
                render::invocation(&envelope.payload);
            }
            if let Some(recorder) = &mut self.recorder {
                recorder.record_invocation(&envelope.payload);
            }
        }
        while let Some(Ok(envelope)) = self.measurements.try_recv() {
            self.session_stats.record_measurement(&envelope.payload);
            if self.verbose {
                render::measurement(&envelope.payload);
            }
            if let Some(recorder) = &mut self.recorder {
                recorder.record_measurement(&envelope.payload);
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
