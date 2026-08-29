//! [`AgentLoop`]: the [`Agent`] implementation.

use std::sync::Arc;

use aik_api::agent::{Agent, AgentDescriptor, AgentId, AgentRequest, AgentResponse, AgentUpdate};
use aik_api::context::ContextCompactor;
use aik_api::context::{ContextStore, TokenCounter};
use aik_api::execution::ExecutionContext;
use aik_api::model::ModelProvider;
use aik_api::quota::QuotaGuard;
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::Result;
use aik_core::clock::{SharedClock, SystemClock};
use aik_core::event::EventBus;
use aik_core::id::ComponentId;
use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::run::{Run, Wiring};
use crate::settings::AgentLoopSettings;

/// A [`TokenCounter`] used only when nobody supplied a real one.
///
/// [`AgentLoop`] estimates request cost for [`RequestMeasured`](aik_api::measurement::RequestMeasured)
/// whether or not a caller cares about token accounting elsewhere, so it needs *some*
/// counter unconditionally. This mirrors
/// [`aik_context::HeuristicTokenCounter`](https://docs.rs/aik-context)'s divisor exactly,
/// but is reimplemented here rather than depended on: `aik-agent` has no other reason to
/// depend on `aik-context`, and duplicating one four-line heuristic is a smaller cost than
/// a crate dependency taken for it alone. [`AgentLoop::with_token_counter`] replaces this
/// with the run's real counter whenever one is available — in practice, always, once wired
/// through [`AgentComponent`](crate::AgentComponent).
#[derive(Debug, Clone, Copy)]
struct FallbackTokenCounter;

impl TokenCounter for FallbackTokenCounter {
    fn count_text(&self, text: &str) -> u64 {
        (text.len() as u64).div_ceil(4)
    }
}

/// The execution-context attribute naming the agent a run belongs to.
///
/// Set by the loop from its own identity, never from a request and never from a message. A
/// policy rule may match on it — [`ExecutionContext::attributes`] is one of the two places a
/// rule's `context` constraint is resolved against — which is exactly why nothing a caller
/// or a model supplies is ever allowed to reach it.
pub const AGENT_ATTRIBUTE: &str = "aik.agent";

/// The execution-context attribute naming the session a run belongs to.
///
/// Taken from [`AgentRequest::session`], so it is the caller's, not the model's. It is what
/// ties the model calls and tool invocations of one conversation together in a trace, what
/// lets a policy rule be scoped to a session, and what the tool registry reads to decide
/// which conversation's [trust](aik_api::provenance) a tool call inherits.
///
/// Defined in `aik-api` rather than here, because the loop that writes it and the registry
/// that reads it do not depend on each other and must still agree.
pub use aik_api::agent::SESSION_ATTRIBUTE;

/// A model/tool loop built from the kernel's existing primitives.
///
/// One run is: record the input, assemble a bounded window from the
/// [`ContextStore`], ask the [`ModelProvider`], run whatever tools it asked for through the
/// [`ToolRegistry`], record the results, repeat — until a turn comes back without a tool
/// call, which is the end.
///
/// # What it adds, and what it deliberately does not
///
/// The loop implements no policy, no confinement and no auditing of its own. Every one of
/// those already exists behind a contract, and the loop's job is to route through them
/// rather than around them:
///
/// | Concern | Where it is handled |
/// |---|---|
/// | May this tool run at all? | [`ToolRegistry::invoke`] → `PolicyEngine` |
/// | May it touch this resource? | `Tool::planned_resources` → the same engine |
/// | Does a human need to say yes? | `Decision::RequireApproval` → `ApprovalSink` |
/// | What is recorded about it? | [`ToolInvoked`](aik_api::audit::ToolInvoked) and friends |
/// | What does the model remember? | [`ContextStore`] |
/// | What does the model get sent? | [`ContextStore::window`] under a [budget](AgentLoopSettings::budget) |
///
/// It adds exactly two rules of its own, both about *bounding*, and both fixed before the
/// conversation starts: a run may take at most
/// [`max_turns`](AgentLoopSettings::max_turns) model turns and invoke at most
/// [`max_tool_calls`](AgentLoopSettings::max_tool_calls) tools, and it may only invoke tools
/// in the set it was given.
///
/// # The trust boundary
///
/// Everything a model produces is data. It reaches exactly three places, and nowhere else:
///
/// * the transcript, as a [`ContextEntry`](aik_api::context::ContextEntry) whose
///   attribution, session, ordering, timestamp and `pinned` flag are all assigned by the
///   store from the [`ExecutionContext`] — so a model can influence what a record *says* and
///   nothing about what it *is*;
/// * a tool's arguments, passed to [`ToolRegistry::invoke`] verbatim and never inspected,
///   rewritten or mined for resource identifiers by the loop, because deriving a resource
///   from arbitrary arguments is the tool's job and the registry says why;
/// * a `call_id`, echoed back into the transcript so a result can be matched to its call.
///   It is a correlation token inside the conversation and is never used as a
///   [`CorrelationId`](aik_core::CorrelationId), a principal, a session or an audit field.
///
/// What a model emits can therefore never change who the run is acting as, which session it
/// is writing to, which model it talks to, which tools exist, how much it may spend, or when
/// it stops.
///
/// [`AgentRequest::context`] is *not* merged into the execution context either, and that is
/// a security decision rather than an omission. A policy rule's `context` constraint is
/// resolved against [`ExecutionContext::attributes`], so anything that lands there can widen
/// or narrow what the run is permitted to do; `AgentRequest::context` is arbitrary
/// caller-supplied JSON that may well be a relay of model output. The only attributes the
/// loop sets are [`AGENT_ATTRIBUTE`] and [`SESSION_ATTRIBUTE`], from its own identity and
/// the caller's session id.
///
/// # Cancellation and deadlines
///
/// The caller's context is propagated, never replaced: each model call and each tool
/// invocation gets a [`child`](ExecutionContext::child) of it, keeping the correlation id,
/// principal and deadline while getting its own cancellation token. The loop checks for
/// cancellation and deadline expiry before every action — including between announcing a
/// tool call and running it — so an expired run never starts anything new. It does not wrap
/// model calls or tool invocations in a timeout: interrupting the work is the provider's and
/// the tool's own obligation, and a wrapper that merely stopped waiting would look like
/// cancellation without being it.
///
/// A run that stops for any reason — cancelled, out of time, out of budget — closes off, in
/// the transcript, every tool call it will now not make. An assistant turn asking for a tool
/// with no result following it is a conversation most providers reject outright, so leaving
/// one behind would mean a stopped run had quietly poisoned the session for whoever resumes
/// it. The one case that cannot be tidied is a caller that *drops* a stream mid-run: dropping
/// a future cannot await anything, so nothing can be written. Cancel the context instead of
/// dropping the stream if the session is to be continued afterwards.
pub struct AgentLoop {
    id: AgentId,
    description: Option<String>,
    models: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolRegistry>,
    context: Arc<dyn ContextStore>,
    clock: SharedClock,
    settings: AgentLoopSettings,
    allowed: Option<Vec<ToolName>>,
    /// Used only to estimate request cost for [`RequestMeasured`](aik_api::measurement::RequestMeasured);
    /// see [`AgentLoop::with_token_counter`].
    counter: Arc<dyn TokenCounter>,
    /// Where [`RequestMeasured`](aik_api::measurement::RequestMeasured) is published;
    /// see [`AgentLoop::with_events`].
    events: Option<(EventBus, ComponentId)>,
    /// What the loop asks for room when a window starts dropping records, if anything;
    /// see [`AgentLoop::with_compactor`].
    compactor: Option<Arc<dyn ContextCompactor>>,
    /// What the loop asks before every turn, and tells afterwards; see
    /// [`AgentLoop::with_quota`].
    quota: Option<Arc<dyn QuotaGuard>>,
}

impl std::fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoop")
            .field("id", &self.id)
            .field("model", &self.settings.model)
            .field("max_turns", &self.settings.max_turns)
            .field("max_tool_calls", &self.settings.max_tool_calls)
            .field("tools", &self.allowed)
            .field("compacts", &self.compactor.is_some())
            .field("metered", &self.quota.is_some())
            .finish_non_exhaustive()
    }
}

impl AgentLoop {
    /// Creates an agent wired to a model provider, a tool registry and a context store.
    ///
    /// The three are supplied rather than looked up, so a loop can be built and tested
    /// without a kernel; [`AgentComponent`](crate::AgentComponent) is the wiring that
    /// resolves them from the registry.
    pub fn new(
        id: impl Into<AgentId>,
        models: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolRegistry>,
        context: Arc<dyn ContextStore>,
        settings: AgentLoopSettings,
    ) -> Self {
        Self {
            id: id.into(),
            description: None,
            models,
            tools,
            context,
            clock: Arc::new(SystemClock),
            settings,
            allowed: None,
            counter: Arc::new(FallbackTokenCounter),
            events: None,
            compactor: None,
            quota: None,
        }
    }

    /// Describes the agent, for a catalogue or a UI.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Restricts the agent to a fixed set of tools.
    ///
    /// Without this the agent offers whatever [`ToolRegistry::list`] reports. With it, the
    /// run's tool set is the intersection of that list and this one, and a model asking for
    /// anything else is told the tool does not exist rather than having the request
    /// forwarded. This narrows what an agent can reach and never widens it: a tool named
    /// here that the registry does not have stays absent, and a tool named here that policy
    /// refuses is still refused.
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = ToolName>) -> Self {
        self.allowed = Some(tools.into_iter().collect());
        self
    }

    /// Overrides the clock used for deadline checks. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Uses a specific [`TokenCounter`] to estimate request cost for
    /// [`RequestMeasured`](aik_api::measurement::RequestMeasured), instead of the internal
    /// fallback.
    ///
    /// Purely observational: this affects only the numbers reported on
    /// [`RequestMeasured`](aik_api::measurement::RequestMeasured), never budgeting,
    /// eviction, or anything a model or a tool sees.
    /// [`AgentComponent`](crate::AgentComponent) calls this with the same counter the
    /// [`ContextStore`] itself uses, so the two report consistent numbers for the same
    /// window.
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = counter;
        self
    }

    /// Publishes [`RequestMeasured`](aik_api::measurement::RequestMeasured) on `events`,
    /// attributed to `source`, once per model turn.
    ///
    /// Without this, the loop still computes every estimate — it costs nothing extra not
    /// to publish what was already computed — it simply publishes nothing. See
    /// [`aik_api::measurement`] for exactly what the event carries and why.
    #[must_use]
    pub fn with_events(mut self, events: EventBus, source: ComponentId) -> Self {
        self.events = Some((events, source));
        self
    }

    /// Asks `compactor` for room whenever a window would otherwise drop records.
    ///
    /// Without one, an overflowing session behaves exactly as it always has: the budget
    /// evicts the oldest records deterministically and the model is simply no longer told
    /// about them. With one, the loop asks it to replace those records with a recap of them
    /// *before* the turn is taken, so what the model loses is the wording rather than the
    /// substance.
    ///
    /// The loop contributes the trigger and nothing else. It does not summarise, hold a
    /// prompt, or decide what a recap should say — see
    /// [`ContextCompactor`](aik_api::context::ContextCompactor), and
    /// [`aik-summary`](../aik_summary/index.html) for the implementation this exists for.
    ///
    /// Compaction is best-effort, and deliberately so: a compactor that fails has cost the
    /// run a model call, and failing the user's request over it would make an agent with
    /// summarisation *less* reliable than one without. A failed or fruitless attempt is
    /// logged, disables further attempts for the rest of the run, and leaves the run to
    /// continue on the deterministically shortened window it already had. Cancellation and
    /// deadlines are the exception — those end the run, because they mean the run is over.
    #[must_use]
    pub fn with_compactor(mut self, compactor: Arc<dyn ContextCompactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Asks `quota` before every model turn, and tells it what the turn cost afterwards.
    ///
    /// The bounds in [`AgentLoopSettings`] stop one run; this stops a principal, across every
    /// run it starts. Without one, a deployment has per-run bounds only, exactly as it did
    /// before quotas existed.
    ///
    /// # What the loop guarantees about it
    ///
    /// * The check happens **before** the request is assembled, so an exhausted budget costs
    ///   nothing at all — not a window assembly, not a compaction, not a token.
    /// * The charge is recorded **after** the response, from
    ///   [`CompletionResponse::usage`](aik_api::model::CompletionResponse::usage) when the
    ///   provider reports it and from this run's own [`TokenCounter`] when it does not, marked
    ///   as an estimate either way. A provider that reports nothing would otherwise charge
    ///   zero for ever, which would make a token or cost ceiling silently unreachable.
    /// * A guard that **fails** ends the run. That is deliberate for both directions: a check
    ///   that cannot be answered is not a budget, and spend that cannot be recorded is spend
    ///   with no account of it. The turn that was already taken stays in the transcript, so
    ///   nothing is lost — the run simply does not start another.
    /// * Nothing here is derived from model output. The principal comes from the run's
    ///   [`ExecutionContext`], the model from the run's settings, the figures from the
    ///   provider or the counter.
    #[must_use]
    pub fn with_quota(mut self, quota: Arc<dyn QuotaGuard>) -> Self {
        self.quota = Some(quota);
        self
    }

    /// Starts a run, deriving its execution context from the caller's.
    fn begin(&self, request: AgentRequest, cx: &ExecutionContext) -> Run {
        let session = request.session;
        // A child context: same correlation, principal and deadline, its own cancellation.
        // The attributes are the loop's own identity and the caller's session — never
        // `request.context`, which is arbitrary caller-supplied JSON.
        let cx = cx
            .child()
            .with_attribute(AGENT_ATTRIBUTE, self.id.as_str())
            .with_attribute(SESSION_ATTRIBUTE, session.to_string());

        Run::new(
            Wiring {
                models: self.models.clone(),
                tools: self.tools.clone(),
                context: self.context.clone(),
                clock: self.clock.clone(),
                settings: self.settings.clone(),
                counter: self.counter.clone(),
                events: self.events.clone(),
                allowed: self.allowed.clone(),
                compactor: self.compactor.clone(),
                quota: self.quota.clone(),
            },
            session,
            request.input,
            cx,
        )
    }
}

#[async_trait]
impl Agent for AgentLoop {
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            id: self.id.clone(),
            description: self.description.clone(),
            tools: self.allowed.clone().unwrap_or_default(),
        }
    }

    async fn stream(
        &self,
        request: AgentRequest,
        cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<AgentUpdate>>> {
        let mut run = self.begin(request, cx);

        // The run lives inside the generator, so dropping the stream drops the run: a
        // consumer that stops listening stops the loop, rather than leaving it going in a
        // detached task.
        Ok(Box::pin(async_stream::stream! {
            loop {
                match run.advance().await {
                    Ok(updates) => {
                        let mut finished = false;
                        for update in updates {
                            finished |= matches!(update, AgentUpdate::Finished(_));
                            yield Ok(update);
                        }
                        if finished {
                            break;
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        break;
                    }
                }
            }
        }))
    }

    async fn run(&self, request: AgentRequest, cx: &ExecutionContext) -> Result<AgentResponse> {
        let mut run = self.begin(request, cx);
        loop {
            for update in run.advance().await? {
                if let AgentUpdate::Finished(response) = update {
                    return Ok(response);
                }
            }
        }
    }
}
