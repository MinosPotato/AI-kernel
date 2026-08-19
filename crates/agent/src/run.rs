//! One run of the loop, as an explicit state machine.
//!
//! The loop is written as a [`Run`] that is [advanced](Run::advance) one action at a time,
//! rather than as a single `while` loop, for one reason: [`Agent`](aik_api::agent::Agent)
//! has both a streaming and a blocking form, and a state machine lets both drive the *same*
//! code. [`Agent::run`](aik_api::agent::Agent::run) advances until it sees
//! [`AgentUpdate::Finished`]; [`Agent::stream`](aik_api::agent::Agent::stream) advances
//! inside a generator and yields what each step produced. There is no second implementation
//! to keep in step, and in particular no second place where authorization could be skipped.
//!
//! One action is: prepare the session, take a model turn, announce a tool call, or run one
//! tool. Splitting "announce" from "run" is what lets a frontend show that a tool is about
//! to run before it does — and, less cosmetically, puts a cancellation check between the
//! model asking for something and the system doing it.

use std::collections::VecDeque;
use std::sync::Arc;

use aik_api::agent::{AgentResponse, AgentUpdate, SessionId};
use aik_api::context::{ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionRequest, ContentPart, FinishReason, Message, ModelProvider, Role, Usage,
};
use aik_api::tool::{ToolCall, ToolName, ToolOutcome, ToolRegistry};
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::{Error, ErrorKind, Result};
use serde_json::json;

use crate::settings::AgentLoopSettings;

/// The services and settings a run borrows from the agent that started it.
pub(crate) struct Wiring {
    pub(crate) models: Arc<dyn ModelProvider>,
    pub(crate) tools: Arc<dyn ToolRegistry>,
    pub(crate) context: Arc<dyn ContextStore>,
    pub(crate) clock: SharedClock,
    pub(crate) settings: AgentLoopSettings,
    /// The tool names this agent may use, or `None` for every tool the registry lists.
    pub(crate) allowed: Option<Vec<ToolName>>,
}

/// One conversation turn-taking session, advanced one action at a time.
pub(crate) struct Run {
    wiring: Wiring,
    session: SessionId,
    /// The caller's input, moved into the transcript by the first action.
    input: Vec<ContentPart>,
    /// The run's own context: same correlation, principal and deadline as the caller's, its
    /// own cancellation token.
    cx: ExecutionContext,
    started: Timestamp,

    /// The tools offered to the model, fixed once at the start of the run.
    available: Vec<ToolName>,
    prepared: bool,
    /// Tool calls the last turn asked for, not yet announced.
    pending: VecDeque<ToolCall>,
    /// The announced call that the next action will run.
    running: Option<ToolCall>,

    turns: usize,
    tool_calls: usize,
    usage: Usage,
    usage_reported: bool,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("session", &self.session)
            .field("correlation", &self.cx.correlation)
            .field("turns", &self.turns)
            .field("tool_calls", &self.tool_calls)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Run {
    pub(crate) fn new(
        wiring: Wiring,
        session: SessionId,
        input: Vec<ContentPart>,
        cx: ExecutionContext,
    ) -> Self {
        let started = wiring.clock.now();
        Self {
            wiring,
            session,
            input,
            cx,
            started,
            available: Vec::new(),
            prepared: false,
            pending: VecDeque::new(),
            running: None,
            turns: 0,
            tool_calls: 0,
            usage: Usage::default(),
            usage_reported: false,
        }
    }

    /// Performs the next action, returning what it produced.
    ///
    /// The run is over once the returned updates contain [`AgentUpdate::Finished`]; calling
    /// this again afterwards would start another model turn, so callers must stop.
    pub(crate) async fn advance(&mut self) -> Result<Vec<AgentUpdate>> {
        match self.step().await {
            Ok(updates) => Ok(updates),
            Err(error) => {
                self.abandon_outstanding_calls(&error).await;
                Err(error)
            }
        }
    }

    /// The next action itself, before anything is done about it having failed.
    async fn step(&mut self) -> Result<Vec<AgentUpdate>> {
        self.guard()?;

        if !self.prepared {
            self.prepare().await?;
            self.prepared = true;
        }

        if let Some(call) = self.running.take() {
            return self.call_tool(call).await;
        }
        if let Some(call) = self.pending.pop_front() {
            return self.announce(call);
        }
        self.turn().await
    }

    /// Answers, in the transcript, every tool call this run will now never make.
    ///
    /// An assistant turn that asks for a tool and is followed by no result for it is a
    /// malformed conversation — most providers reject it outright — so a run that stops
    /// between the asking and the answering would poison the session for whoever resumes it.
    /// Whatever stopped it, the outstanding calls are closed off with the reason.
    ///
    /// Best effort on purpose: if the store is the thing that failed, there is nowhere to
    /// record this and nothing further to be done about it, and the original failure is the
    /// one the caller needs to see.
    async fn abandon_outstanding_calls(&mut self, reason: &Error) {
        let outstanding: Vec<ToolCall> = self
            .running
            .take()
            .into_iter()
            .chain(self.pending.drain(..))
            .collect();

        for call in outstanding {
            let entry = ContextEntry::new(Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    call_id: call.call_id,
                    content: json!({
                        "kind": classify(reason.kind()),
                        "message": format!("the run stopped before this call was made: {reason}"),
                    }),
                    is_error: true,
                }],
                name: None,
            });
            if let Err(error) = self.append(entry).await {
                tracing::debug!(%error, "could not record an abandoned tool call");
            }
        }
    }

    /// Fails the run if it has been cancelled or has run out of time.
    ///
    /// This is checked before every action rather than by wrapping the actions in a timeout.
    /// A wrapper that stopped *waiting* for a model call or a tool without stopping the work
    /// itself would look like cancellation without being it; honouring the deadline inside
    /// the operation is the provider's and the tool's own obligation, and both
    /// [`ModelProvider`] and [`Tool`](aik_api::tool::Tool) say so. What the loop adds is the
    /// guarantee that an expired run does not start anything new.
    fn guard(&self) -> Result<()> {
        if self.cx.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let now = self.wiring.clock.now();
        if self.cx.deadline.is_some_and(|deadline| now >= deadline) {
            return Err(Error::Timeout(now.saturating_since(self.started)));
        }
        Ok(())
    }

    /// Fixes the tool set and records the input.
    ///
    /// The tool set is resolved once, here, rather than per turn: it is part of the run's
    /// trusted metadata, and re-reading it each turn would make what an agent may do depend
    /// on when it asked.
    async fn prepare(&mut self) -> Result<()> {
        let mut available: Vec<ToolName> = self
            .wiring
            .tools
            .list(&self.cx)
            .await?
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        if let Some(allowed) = &self.wiring.allowed {
            available.retain(|name| allowed.contains(name));
        }
        self.available = available;

        // A system prompt belongs to the session, not to the turn. `stats` returning `None`
        // is what identifies a session nobody has written to yet; for a session owned by
        // another principal it returns `PermissionDenied` instead, and the run stops here
        // rather than leaking a prompt into someone else's transcript.
        if let Some(prompt) = self.wiring.settings.system_prompt.clone() {
            let fresh = self
                .wiring
                .context
                .stats(&self.session, &self.cx)
                .await?
                .is_none();
            if fresh {
                self.append(ContextEntry::new(Message::text(Role::System, prompt)).pinned())
                    .await?;
            }
        }

        let input = std::mem::take(&mut self.input);
        self.append(ContextEntry::new(Message {
            role: Role::User,
            content: input,
            name: None,
        }))
        .await?;

        Ok(())
    }

    /// Takes one model turn against a freshly assembled window.
    async fn turn(&mut self) -> Result<Vec<AgentUpdate>> {
        if self.turns >= self.wiring.settings.max_turns {
            return Err(Error::other(format!(
                "agent run in session `{}` reached its limit of {} model turns \
                 without producing a final response",
                self.session, self.wiring.settings.max_turns
            )));
        }

        // Recomputed every turn, from the transcript, under the run's budget. Nothing that
        // was elided or evicted here is lost: it stays in the store, addressable by record
        // id.
        let window = self
            .wiring
            .context
            .window(&self.session, &self.wiring.settings.budget, &self.cx)
            .await?;

        let request = CompletionRequest {
            model: self.wiring.settings.model.clone(),
            messages: window.messages,
            tools: self.available.clone(),
            parameters: self.wiring.settings.parameters.clone(),
        };

        let response = self
            .wiring
            .models
            .complete(request, &self.cx.child())
            .await?;
        self.turns += 1;
        if let Some(usage) = response.usage {
            self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
            self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
            self.usage_reported = true;
        }

        if response.finish_reason == FinishReason::Cancelled {
            return Err(Error::Cancelled);
        }

        // Recorded before any tool runs, so the call is always in the transcript ahead of
        // the result that answers it — which is the invariant window assembly relies on.
        self.append(ContextEntry::new(response.message.clone()))
            .await?;

        let mut updates = Vec::with_capacity(response.message.content.len());
        let mut output = Vec::with_capacity(response.message.content.len());
        for part in response.message.content {
            match part {
                ContentPart::ToolCall(call) => self.pending.push_back(call),
                other => {
                    updates.push(AgentUpdate::Content(other.clone()));
                    output.push(other);
                }
            }
        }

        // Termination is decided by what the model actually emitted, not by what it said
        // about itself: a turn with no tool calls in it is the end of the run, whatever
        // `finish_reason` claims.
        if self.pending.is_empty() {
            updates.push(AgentUpdate::Finished(AgentResponse {
                session: self.session,
                output,
                usage: self.usage_reported.then_some(self.usage),
            }));
        }

        Ok(updates)
    }

    /// Accepts a tool call against the run's budget and announces it.
    fn announce(&mut self, call: ToolCall) -> Result<Vec<AgentUpdate>> {
        if self.tool_calls >= self.wiring.settings.max_tool_calls {
            // Put it back so it is closed off in the transcript along with the rest.
            self.pending.push_front(call);
            return Err(Error::other(format!(
                "agent run in session `{}` reached its limit of {} tool calls",
                self.session, self.wiring.settings.max_tool_calls
            )));
        }
        self.tool_calls += 1;
        self.running = Some(call.clone());
        Ok(vec![AgentUpdate::ToolCall(call)])
    }

    /// Runs one announced tool call and records what it produced.
    ///
    /// The only path to a tool is [`ToolRegistry::invoke`], which resolves the tool's
    /// permissions and its planned resources before anything executes. The loop adds one
    /// restriction of its own and takes none away: a name outside the run's tool set never
    /// reaches the registry at all.
    async fn call_tool(&mut self, call: ToolCall) -> Result<Vec<AgentUpdate>> {
        let outcome = if self.available.contains(&call.name) {
            match self
                .wiring
                .tools
                .invoke(&call.name, call.arguments.clone(), &self.cx.child())
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => match self.guard() {
                    // A refusal, a bad argument or a broken tool is something the model
                    // should see and can react to, so it becomes an error result rather than
                    // ending the run.
                    Ok(()) => {
                        tracing::debug!(
                            tool = %call.name,
                            kind = ?error.kind(),
                            "tool call failed; reporting it to the model",
                        );
                        failed(&error)
                    }
                    // The run's own lifecycle is not something to tell the model about: if it
                    // has been cancelled or has run out of time, stop instead of going round
                    // again. Handing the call back leaves it to be closed off in the
                    // transcript with everything else that will now not happen.
                    Err(stopped) => {
                        self.running = Some(call);
                        return Err(stopped);
                    }
                },
            }
        } else {
            tracing::debug!(tool = %call.name, "model asked for a tool this agent does not have");
            unavailable(&call.name)
        };

        self.append(ContextEntry::new(Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                call_id: call.call_id.clone(),
                content: outcome.output.clone(),
                is_error: outcome.is_error,
            }],
            name: None,
        }))
        .await?;

        Ok(vec![AgentUpdate::ToolResult {
            call_id: call.call_id,
            outcome,
        }])
    }

    /// Appends to the run's own session, under the run's own principal.
    ///
    /// Every append in the loop goes through here, so there is one place where the session
    /// and the execution context are chosen — and neither is ever taken from a message.
    async fn append(&self, entry: ContextEntry) -> Result<()> {
        self.wiring
            .context
            .append(&self.session, entry, &self.cx)
            .await
            .map(|_| ())
    }
}

/// The result a tool that could not be run reports back to the model.
///
/// Both fields are model-visible, which is the point: a model that is told only that
/// "something failed" retries the identical call. `kind` is the kernel's stable
/// classification; `message` is the error's own text, which for a denial is the reason the
/// policy engine authored. Policy authors should write reasons on the assumption that a
/// model reads them.
fn failed(error: &Error) -> ToolOutcome {
    ToolOutcome::error(json!({
        "kind": classify(error.kind()),
        "message": error.to_string(),
    }))
}

/// The result reported for a tool this agent was never given.
fn unavailable(name: &ToolName) -> ToolOutcome {
    ToolOutcome::error(json!({
        "kind": classify(ErrorKind::NotFound),
        "message": format!("no tool named `{name}` is available to this agent"),
    }))
}

/// Renders an [`ErrorKind`] the way [`InvocationOutcome::Failed`] does, so a result the
/// model sees and the audit event for the same call agree on what went wrong.
///
/// [`InvocationOutcome::Failed`]: aik_api::audit::InvocationOutcome::Failed
fn classify(kind: ErrorKind) -> String {
    format!("{kind:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_classified_the_way_audit_events_are() {
        assert_eq!(classify(ErrorKind::Permission), "permission");
        assert_eq!(classify(ErrorKind::NotFound), "notfound");
        assert_eq!(classify(ErrorKind::Cancelled), "cancelled");
    }

    #[test]
    fn a_failed_call_tells_the_model_both_what_and_why() {
        let outcome = failed(&Error::PermissionDenied("outside the workspace".into()));
        assert!(outcome.is_error);
        assert_eq!(outcome.output["kind"], json!("permission"));
        assert!(
            outcome.output["message"]
                .as_str()
                .expect("a message")
                .contains("outside the workspace")
        );
    }

    #[test]
    fn an_unavailable_tool_is_reported_as_not_found() {
        let outcome = unavailable(&ToolName::new("ghost"));
        assert!(outcome.is_error);
        assert_eq!(outcome.output["kind"], json!("notfound"));
    }
}
