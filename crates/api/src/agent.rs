//! Agent contracts.
//!
//! An [`Agent`] takes input and produces output, possibly over a long time, possibly
//! calling tools and models along the way. The kernel does not define how — no loop, no
//! planner, no prompt structure. It defines the boundary: a request in, a stream of
//! [`AgentUpdate`]s out.
//!
//! The streaming interface is the primary one because everything user-facing needs partial
//! output, and because a long-running agent that only reports when finished is unusable in
//! a desktop shell. [`Agent::run`] exists for callers that genuinely only want the result.

use aik_core::Result;
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;
use crate::model::{ContentPart, Usage};
use crate::tool::{ToolCall, ToolName, ToolOutcome};

aik_core::string_id! {
    /// Names an agent.
    pub AgentId
}

aik_core::uuid_id! {
    /// Identifies one conversation with an agent.
    pub SessionId
}

/// What an agent is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescriptor {
    /// The agent's name.
    pub id: AgentId,
    /// A human-readable summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tools this agent may use, if it declares a fixed set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolName>,
}

/// Input to an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRequest {
    /// The conversation this belongs to.
    ///
    /// Continuity is the agent's responsibility: the kernel does not store sessions, it
    /// only names them.
    pub session: SessionId,
    /// The input.
    pub input: Vec<ContentPart>,
    /// Caller-supplied context: the active window, the channel, the calling frontend.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context: Value,
}

impl AgentRequest {
    /// Creates a text request in a new session.
    pub fn text(input: impl Into<String>) -> Self {
        Self {
            session: SessionId::new(),
            input: vec![ContentPart::text(input)],
            context: Value::Null,
        }
    }
}

/// Something an agent did or produced.
///
/// Tool calls and their outcomes are reported as updates rather than hidden inside the
/// agent, so a UI can show what the system is doing and a permission layer has something
/// to attach approval to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentUpdate {
    /// Incremental output.
    Content(ContentPart),
    /// The agent's own account of what it is doing, for display.
    Status {
        /// A short description.
        message: String,
    },
    /// The agent is about to run a tool.
    ToolCall(ToolCall),
    /// A tool finished.
    ToolResult {
        /// Which call finished.
        call_id: String,
        /// What it produced.
        outcome: ToolOutcome,
    },
    /// The agent finished.
    Finished(AgentResponse),
}

/// An agent's final output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResponse {
    /// The conversation this belongs to.
    pub session: SessionId,
    /// The output.
    pub output: Vec<ContentPart>,
    /// Token usage across the whole run, if tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Something that can be asked to do work.
#[async_trait]
pub trait Agent: Send + Sync + 'static {
    /// Describes the agent.
    fn descriptor(&self) -> AgentDescriptor;

    /// Runs the agent, reporting progress as it goes.
    ///
    /// The stream ends after [`AgentUpdate::Finished`], or when `cx` is cancelled.
    async fn stream(
        &self,
        request: AgentRequest,
        cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<AgentUpdate>>>;

    /// Runs the agent and waits for the final response.
    async fn run(&self, request: AgentRequest, cx: &ExecutionContext) -> Result<AgentResponse>;
}
