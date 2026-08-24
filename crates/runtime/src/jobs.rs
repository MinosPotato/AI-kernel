//! Running an agent turn when a scheduled job fires.
//!
//! The scheduler stores jobs, decides when they are due and calls a
//! [`aik_api::scheduler::JobHandler`]. It ships with no handler at all, on
//! purpose: what a firing *does* is not the scheduler's business, and a scheduler that knew
//! about agents would be a scheduler that could only ever run agents. This is the handler
//! that closes the loop for the one thing this system is for — asking the assistant
//! something without anybody being there to type it.
//!
//! # Whose authority a firing carries
//!
//! Not the client that scheduled it, and not the operator. The scheduler derives the firing's
//! principal from the *owner recorded at scheduling time* — `scheduler` acting on behalf of
//! that owner, see [`aik_scheduler`]'s own documentation — and hands it to this handler in the
//! [`ExecutionContext`]. This handler passes that context straight through to
//! [`aik_api::agent::Agent::run`] and constructs nothing of its own.
//!
//! That matters more here than anywhere else in the system, because a firing is the one
//! operation with no human anywhere near it. Three consequences, all deliberate:
//!
//! * **Every tool call is still gated.** The agent reaches tools only through
//!   [`ToolRegistry`](aik_api::tool::ToolRegistry), which consults the policy engine with the
//!   firing's principal. A policy that distinguishes "alice asked for this" from "a job is
//!   doing this for alice" sees the difference, because the two are different principals.
//! * **An approval with nobody to ask is a refusal.** The broker refuses when no
//!   [`ApprovalGate`](aik_approval::ApprovalGate) is attached, so a firing at 3am with no
//!   client connected cannot be approved by default; it is denied, and the denial is audited.
//! * **The payload cannot choose an identity.** It carries a prompt and, optionally, a
//!   session. There is no principal field, no agent field, and no way to add one: the only
//!   identity in play is the one the scheduler stamped.
//!
//! # Which session a firing appends to
//!
//! A payload may name one, and then the context store's own ownership rule applies unchanged
//! — a job whose owner may not act for that session's owner fails, which is the right
//! failure. With no session named, each firing gets a fresh one rather than sharing a
//! long-lived transcript: unattended runs that accumulated into one session would grow a
//! transcript nobody reads until it hit the window budget, and would leak each firing's
//! output into the next.

use std::sync::Arc;

use aik_api::agent::{Agent, AgentRequest, SessionId};
use aik_api::execution::ExecutionContext;
use aik_api::model::ContentPart;
use aik_api::scheduler::{JobHandler, JobSpec};
use aik_core::prelude::*;
use serde::Deserialize;
use serde_json::Value;

/// The component id used when none is given explicitly.
///
/// This is what a [`JobSpec::handler`] names, so it is part of the durable format: changing
/// it strands every stored job that referenced the old one.
pub const DEFAULT_COMPONENT_ID: &str = "agent.jobs";

/// What an agent job's payload has to say.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentJobPayload {
    /// What to ask.
    pub prompt: String,
    /// The transcript to append to, or `None` for a fresh one each firing.
    #[serde(default)]
    pub session: Option<SessionId>,
}

impl AgentJobPayload {
    /// Reads a payload, naming what is wrong with one that will not do.
    ///
    /// Unknown fields are rejected rather than ignored. A payload is stored for as long as
    /// the job exists and is written by hand or by a client; a misspelled `sesion` that
    /// silently became "a fresh session every firing" is exactly the kind of quiet difference
    /// nobody notices until they go looking for a transcript that was never appended to.
    pub fn parse(payload: &Value) -> Result<Self> {
        if payload.is_null() {
            return Err(Error::InvalidArgument(
                "an agent job needs a payload naming what to ask, e.g. {\"prompt\": \"...\"}"
                    .to_owned(),
            ));
        }
        let parsed: Self = serde_json::from_value(payload.clone())
            .map_err(|error| Error::InvalidArgument(format!("this agent job's payload {error}")))?;
        if parsed.prompt.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "an agent job's `prompt` must not be empty".to_owned(),
            ));
        }
        Ok(parsed)
    }
}

/// Runs one agent turn per firing.
pub struct AgentJobHandler {
    agent: Arc<dyn Agent>,
}

impl std::fmt::Debug for AgentJobHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentJobHandler")
            .field("agent", &self.agent.descriptor().id)
            .finish()
    }
}

impl AgentJobHandler {
    /// Wraps the agent every firing is put to.
    pub fn new(agent: Arc<dyn Agent>) -> Self {
        Self { agent }
    }
}

#[async_trait]
impl JobHandler for AgentJobHandler {
    async fn run(&self, job: &JobSpec, cx: &ExecutionContext) -> Result<()> {
        let payload = AgentJobPayload::parse(&job.payload)?;
        let request = AgentRequest {
            // A fresh id per firing, not per job: two firings of the same job are two
            // conversations unless the payload deliberately says otherwise.
            session: payload.session.unwrap_or_default(),
            input: vec![ContentPart::text(payload.prompt)],
            context: Value::Null,
        };

        // `cx` unchanged, deliberately. It carries the firing's principal, its deadline and
        // the scheduler's cancellation token; deriving a context here would be inventing an
        // authority, a lifetime, or both.
        self.agent.run(request, cx).await.map(|_| ())
    }
}

/// Publishes an [`AgentJobHandler`] for the kernel's own agent.
///
/// Registered under [`DEFAULT_COMPONENT_ID`] as a `dyn JobHandler`, which is the name a
/// stored [`JobSpec::handler`] refers to. It depends on the agent component, so a kernel
/// wired with this and without an agent refuses to start rather than accepting jobs it could
/// never run.
///
/// The handler is built in `start` rather than `init`: it resolves `dyn Agent`, which the
/// agent component publishes during its own `init`, and scheduler components snapshot every
/// handler during *their* `start`. The kernel orders both by the declared dependency.
#[derive(Debug)]
pub struct AgentJobComponent {
    id: ComponentId,
    agent: ComponentId,
}

impl Default for AgentJobComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentJobComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            agent: ComponentId::new(aik_agent::DEFAULT_COMPONENT_ID),
        }
    }

    /// Registers under a different component id.
    ///
    /// The id is what a stored job names, so changing it is a change to the durable format.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Depends on, and resolves, a differently named agent component.
    #[must_use]
    pub fn with_agent(mut self, agent: impl Into<ComponentId>) -> Self {
        self.agent = agent.into();
        self
    }
}

#[async_trait]
impl Component for AgentJobComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("runs an agent turn when a scheduled job fires")
            .requires(self.agent.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let agent = ctx.service_named::<dyn Agent>(&self.agent)?;
        ctx.provide::<dyn JobHandler>(Arc::new(AgentJobHandler::new(agent)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_payload_needs_a_prompt() {
        let error = AgentJobPayload::parse(&Value::Null).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");

        let error = AgentJobPayload::parse(&json!({ "prompt": "  " })).unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let error = AgentJobPayload::parse(&json!({ "prompt": "hello", "sesion": "x" }))
            .expect_err("an unknown field must not be silently dropped");
        assert!(error.to_string().contains("sesion"), "{error}");
    }

    #[test]
    fn a_payload_cannot_name_a_principal_or_an_agent() {
        for field in ["principal", "owner", "agent", "user"] {
            let payload = json!({ "prompt": "hello", field: "root" });
            assert!(
                AgentJobPayload::parse(&payload).is_err(),
                "`{field}` must not be accepted: a stored payload must not choose an identity",
            );
        }
    }

    #[test]
    fn a_named_session_is_carried_and_an_absent_one_is_not_invented_twice() {
        let session = SessionId::new();
        let parsed = AgentJobPayload::parse(&json!({
            "prompt": "hello",
            "session": session.to_string(),
        }))
        .expect("parsed");
        assert_eq!(parsed.session, Some(session));

        let anonymous = AgentJobPayload::parse(&json!({ "prompt": "hello" })).expect("parsed");
        assert_eq!(anonymous.session, None);
    }
}
