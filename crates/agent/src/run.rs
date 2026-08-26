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
use std::time::{Duration, Instant};

use aik_api::agent::{AgentResponse, AgentUpdate, SessionId};
use aik_api::context::{
    ContextCompactor, ContextEntry, ContextStore, ContextUsage, ContextWindow, TokenCounter,
};
use aik_api::execution::ExecutionContext;
use aik_api::measurement::{RequestEstimate, RequestMeasured};
use aik_api::model::{
    CompletionRequest, ContentPart, FinishReason, Message, ModelProvider, Role, ToolDefinition,
    Usage,
};
use aik_api::tool::{ToolCall, ToolName, ToolOutcome, ToolRegistry, ToolSpec};
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::event::{Envelope, EventBus};
use aik_core::id::ComponentId;
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
    /// Estimates the token cost of what is about to be sent, for [`RequestMeasured`].
    ///
    /// Purely observational: nothing about budgeting or eviction reads this counter. It
    /// exists only so tool-definition and message-breakdown estimates use the same
    /// [`TokenCounter`] the context store's own accounting does, when one has been
    /// supplied; see [`crate::AgentLoop::with_token_counter`].
    pub(crate) counter: Arc<dyn TokenCounter>,
    /// Where [`RequestMeasured`] is published, and under whose component id — `None` if
    /// nobody asked for measurement events, in which case none are published and nothing
    /// else about the run changes.
    pub(crate) events: Option<(EventBus, ComponentId)>,
    /// The tool names this agent may use, or `None` for every tool the registry lists.
    pub(crate) allowed: Option<Vec<ToolName>>,
    /// What the run asks for room when a window starts dropping records, if anything; see
    /// [`crate::AgentLoop::with_compactor`].
    pub(crate) compactor: Option<Arc<dyn ContextCompactor>>,
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
    ///
    /// Held as full [`ToolSpec`](aik_api::tool::ToolSpec)s because the run needs both halves
    /// of them and they must agree: the name it admits a call under, and the description and
    /// schema the model is told about. Only the model-facing subset is ever sent — see
    /// [`ToolDefinition`].
    available: Vec<ToolSpec>,
    prepared: bool,
    /// Tool calls the last turn asked for, not yet announced.
    pending: VecDeque<ToolCall>,
    /// The announced call that the next action will run.
    running: Option<ToolCall>,

    turns: usize,
    tool_calls: usize,
    /// Whether the run will still ask for room.
    ///
    /// Turned off by a compactor that failed or that found nothing to do, so one round of
    /// either costs one attempt and not one per turn. It never turns back on within a run:
    /// what made compaction pointless or impossible this turn is not going to have changed
    /// by the next one.
    compacting: bool,
    usage: Usage,
    usage_reported: bool,
    /// The estimated cost of the caller's input, set once by [`Run::prepare`] and consumed
    /// by the first turn's [`RequestMeasured`] — `None` on every later turn, since a run
    /// appends fresh user input exactly once.
    current_input_tokens: Option<u64>,
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
            compacting: true,
            usage: Usage::default(),
            usage_reported: false,
            current_input_tokens: None,
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
        let mut available: Vec<ToolSpec> = self.wiring.tools.list(&self.cx).await?;
        if let Some(allowed) = &self.wiring.allowed {
            available.retain(|spec| allowed.contains(&spec.name));
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

        // Before the question is recorded rather than after, and this is the one place the
        // order matters: a transcript is append-only, so a recap lands at the *end* of the
        // session. Compacting first puts it behind the input it precedes; compacting later
        // would leave the newest thing in the window being a summary of the oldest, with the
        // user's actual question above it.
        //
        // The window assembled here is only ever assembled to ask whether the session is
        // already losing records, which is why it is not assembled at all unless something
        // could act on the answer.
        if self.compactor().is_some() {
            let dropped = self.window().await?.usage.dropped_records;
            self.make_room(dropped).await?;
        }

        let input = std::mem::take(&mut self.input);
        let user_message = Message {
            role: Role::User,
            content: input,
            name: None,
        };
        // Measured before the message is moved into the store, so `RequestMeasured` can
        // report it against the turn it was actually sent on. This is the only turn of the
        // run that carries fresh user text — see `current_input_tokens`'s own doc comment.
        self.current_input_tokens = Some(self.wiring.counter.count_message(&user_message));
        self.append(ContextEntry::new(user_message)).await?;

        Ok(())
    }

    /// Assembles the model payload for this session under the run's budget.
    fn window(&self) -> impl std::future::Future<Output = Result<ContextWindow>> + '_ {
        self.wiring
            .context
            .window(&self.session, &self.wiring.settings.budget, &self.cx)
    }

    /// Asks the compactor to replace what the budget is about to evict, if there is one.
    ///
    /// Best-effort by design. What compaction buys is a model that remembers the substance
    /// of turns it can no longer be shown; what it costs when it fails is nothing the run
    /// had before it was wired in, since the window still assembles, still evicts oldest
    /// first, and still says what it dropped. So a failure is logged and the run continues —
    /// with one exception, made because it is not really an exception: a cancelled or
    /// expired run has to stop whatever it was doing when it found out.
    ///
    /// Nothing here inspects, edits or even reads what the compactor wrote. The loop asks
    /// for room; what fills it is the compactor's business and the store's.
    async fn make_room(&mut self, dropped_records: usize) -> Result<bool> {
        // Asked for only when the window is actually losing records: compaction costs a
        // model call, and a session that fits its budget has nothing to gain from one.
        if dropped_records == 0 {
            return Ok(false);
        }
        let Some(compactor) = self.compactor() else {
            return Ok(false);
        };

        match compactor
            .compact(&self.session, &self.wiring.settings.budget, &self.cx)
            .await
        {
            Ok(compaction) => {
                if compaction.is_empty() {
                    self.compacting = false;
                    tracing::debug!(
                        session = %self.session,
                        "the session is over budget but has nothing left to compact"
                    );
                    return Ok(false);
                }
                Ok(true)
            }
            Err(error) if matches!(error.kind(), ErrorKind::Cancelled | ErrorKind::Timeout) => {
                Err(error)
            }
            Err(error) => {
                self.compacting = false;
                tracing::warn!(
                    %error,
                    session = %self.session,
                    "could not compact the session; continuing on the shortened window"
                );
                Ok(false)
            }
        }
    }

    /// The compactor this run will still ask, if there is one and asking is still worth it.
    fn compactor(&self) -> Option<Arc<dyn ContextCompactor>> {
        self.compacting
            .then(|| self.wiring.compactor.clone())
            .flatten()
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
        let mut window = self.window().await?;
        // A second chance, for a window that overflowed during the run rather than before
        // it: a few large tool results are enough to do it. At most one round per turn, and
        // none at all once `make_room` has reported that asking is pointless — which is also
        // why the window is only reassembled when something actually changed.
        if self.make_room(window.usage.dropped_records).await? {
            window = self.window().await?;
        }
        let context_usage = window.usage;

        let tool_definitions: Vec<ToolDefinition> =
            self.available.iter().map(ToolDefinition::from).collect();
        // Measured against exactly what is about to be sent — the same window messages and
        // the same tool definitions — before either is moved into the request.
        let estimate = estimate_request(
            &window.messages,
            &tool_definitions,
            self.wiring.counter.as_ref(),
            self.current_input_tokens.take(),
        );

        let request = CompletionRequest {
            model: self.wiring.settings.model.clone(),
            messages: window.messages,
            tools: tool_definitions,
            parameters: self.wiring.settings.parameters.clone(),
        };

        let model_started = Instant::now();
        let response = self
            .wiring
            .models
            .complete(request, &self.cx.child())
            .await?;
        let model_latency = model_started.elapsed();
        self.turns += 1;
        if let Some(usage) = response.usage {
            self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
            self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
            self.usage_reported = true;
        }

        // Purely observational: a missing subscriber changes nothing about the turn that
        // already happened, and this happens after every value it reports has already been
        // produced.
        self.publish_measurement(estimate, context_usage, response.usage, model_latency);

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
        // Set only when the call itself completed but the run must stop anyway — checked
        // after the transcript is written, never before. `tools.invoke` already returned by
        // the time `guard` is consulted below, so the call is not "outstanding" in the sense
        // `abandon_outstanding_calls` means: it has a real, known outcome, and that outcome —
        // not a fabricated "the run stopped before this call was made" — is what belongs in
        // the transcript, whether or not the run goes on to use it.
        let mut stop = None;

        let outcome = if self.available.iter().any(|spec| spec.name == call.name) {
            match self
                .wiring
                .tools
                .invoke(&call.name, call.arguments.clone(), &self.cx.child())
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    match self.guard() {
                        // A refusal, a bad argument or a broken tool is something the model
                        // should see and can react to, so it becomes an error result rather
                        // than ending the run.
                        Ok(()) => tracing::debug!(
                            tool = %call.name,
                            kind = ?error.kind(),
                            "tool call failed; reporting it to the model",
                        ),
                        // The run's own lifecycle is not something to tell the model about —
                        // no further turn will read this result — but the call still
                        // happened and still has a real outcome, so it is recorded exactly
                        // as it would be if the run were continuing.
                        Err(stopped) => {
                            tracing::debug!(
                                tool = %call.name,
                                kind = ?error.kind(),
                                "tool call failed as the run was stopping; recording its real outcome",
                            );
                            stop = Some(stopped);
                        }
                    }
                    failed(&error)
                }
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

        if let Some(stopped) = stop {
            return Err(stopped);
        }

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

    /// Publishes what one model turn cost, if anyone asked to hear about it.
    ///
    /// A no-op with no [`EventBus`] configured — see [`Wiring::events`] — so a run with
    /// nothing subscribed pays only the cost of computing `estimate`, which it needed to
    /// compute anyway for nothing to read. Nothing about the run's own control flow depends
    /// on whether this publishes anything.
    fn publish_measurement(
        &self,
        estimate: RequestEstimate,
        context: ContextUsage,
        provider_usage: Option<Usage>,
        model_latency: Duration,
    ) {
        let Some((events, source)) = &self.wiring.events else {
            return;
        };
        let event = RequestMeasured {
            correlation: self.cx.correlation,
            timestamp: self.wiring.clock.now(),
            session: self.session,
            model: self.wiring.settings.model.clone(),
            turn: self.turns,
            cumulative_tool_calls: self.tool_calls,
            estimate,
            context,
            provider_usage,
            cumulative_provider_usage: self.usage_reported.then_some(self.usage),
            model_latency_ms: millis(model_latency),
        };
        let metadata = events
            .metadata_for::<RequestMeasured>()
            .with_source(source.clone())
            .with_correlation(self.cx.correlation);
        events.publish_envelope(Envelope::new(metadata, event));
    }
}

/// Converts a duration to milliseconds, saturating rather than panicking on an
/// implausibly long one.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Builds the locally estimated breakdown for one request, from exactly the messages and
/// tool definitions that request is about to carry.
///
/// A free function rather than a method: it touches no run state beyond what is passed in,
/// which is what makes it straightforward to reason about and to unit test without
/// constructing a whole [`Run`].
fn estimate_request(
    messages: &[Message],
    tools: &[ToolDefinition],
    counter: &dyn TokenCounter,
    user_input_tokens: Option<u64>,
) -> RequestEstimate {
    let mut system_tokens = 0u64;
    let mut conversation_tokens = 0u64;
    let mut tool_call_tokens = 0u64;
    let mut tool_result_tokens = 0u64;

    for message in messages {
        let cost = counter.count_message(message);
        if message.role == Role::System {
            system_tokens += cost;
            continue;
        }
        conversation_tokens += cost;
        for part in &message.content {
            match part {
                ContentPart::ToolCall(_) => tool_call_tokens += counter.count_part(part),
                ContentPart::ToolResult { .. } => tool_result_tokens += counter.count_part(part),
                _ => {}
            }
        }
    }

    let tool_definition_tokens: u64 = tools
        .iter()
        .map(|definition| {
            counter.count_text(definition.name.as_str())
                + counter.count_text(&definition.description)
                + counter.count_json(&definition.input_schema)
        })
        .sum();

    RequestEstimate {
        system_tokens,
        conversation_tokens,
        user_input_tokens,
        tool_call_tokens,
        tool_result_tokens,
        tool_definition_tokens,
        tools_offered: tools.len(),
        total_tokens: system_tokens + conversation_tokens + tool_definition_tokens,
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
