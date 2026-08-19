//! A minimal agent loop over the AI kernel's primitives.
//!
//! Everything before this crate was a capability in isolation: a policy engine that decides,
//! a registry that enforces, tools that act, a broker that asks a human, a store that
//! remembers, a window that budgets. Each was built and tested on its own. This crate is the
//! first thing that *uses* them together, and it is deliberately the smallest thing that
//! can: it adds no new contract, no new event, no new authorization concept, and no
//! subsystem of its own.
//!
//! # The loop
//!
//! ```text
//!  AgentRequest
//!       │
//!       ▼
//!  ContextStore::append  ── system prompt (pinned, once per session), then the input
//!       │
//!       ▼
//!  ┌─▶ ContextStore::window ── budgeted, recomputed every turn, never stored
//!  │        │
//!  │        ▼
//!  │   ModelProvider::complete
//!  │        │
//!  │        ├── no tool calls ──▶ ContextStore::append ──▶ AgentResponse   (done)
//!  │        │
//!  │        └── tool calls ──▶ ContextStore::append (the calls)
//!  │                              │
//!  │                              ▼
//!  │                     ToolRegistry::invoke
//!  │                       ├ tool-level authorization
//!  │                       ├ resource-level authorization
//!  │                       ├ human approval, if policy asks for it
//!  │                       └ audit events for all of it
//!  │                              │
//!  └──────── ContextStore::append (the results) ◀──┘
//! ```
//!
//! Every arrow is an existing contract. The loop contributes the arrows, not the boxes.
//!
//! # What it is for
//!
//! ```no_run
//! use aik_agent::{AgentComponent, AgentLoopSettings};
//! use aik_api::agent::{Agent, AgentRequest};
//! use aik_api::execution::ExecutionContext;
//! use aik_core::prelude::*;
//!
//! # async fn demo(kernel: Kernel) -> Result<()> {
//! kernel.start().await?;
//!
//! let agent = kernel.context().service::<dyn Agent>()?;
//! let response = agent
//!     .run(AgentRequest::text("what is in my project directory?"), &ExecutionContext::new())
//!     .await?;
//!
//! println!("{:?}", response.output);
//! # Ok(())
//! # }
//! ```
//!
//! # Security
//!
//! The loop is the point where untrusted model output first meets the parts of the system
//! that can do something, so its whole job is to be a *conduit* and never a *decision
//! point*. Concretely:
//!
//! * **Tools are reached only through [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry).**
//!   The loop never holds a `dyn Tool` and has no way to obtain one, so there is no path from
//!   a model's request to a tool that skips capability-level authorization, resource-level
//!   authorization, approval or auditing.
//! * **A tool failure is data, not an exemption.** A denial, a refused approval or a broken
//!   tool becomes an error result the model can see and react to. Nothing about that path
//!   re-runs the tool, downgrades the check, or caches a decision.
//! * **The model cannot widen its own reach.** The run's tool set is fixed before the first
//!   turn; a call naming anything else is answered with "no such tool" without reaching the
//!   registry at all.
//! * **The model cannot forge metadata.** Attribution, session, ordering, timestamps and
//!   pinning are assigned by the [`ContextStore`](aik_api::context::ContextStore) from the
//!   [`ExecutionContext`](aik_api::execution::ExecutionContext); the loop passes model output
//!   in as a payload and never as any of those. See
//!   [`AgentLoop`](AgentLoop#the-trust-boundary) for the full list of where model-produced
//!   bytes are allowed to go.
//! * **Sessions stay isolated.** Every append, window and stat goes through one execution
//!   context carrying one principal, so a run can only read and write the transcript of a
//!   session that principal owns — the store refuses anything else, and the run fails rather
//!   than continuing without its history.
//! * **A run is bounded.** Turns, tool calls, the per-turn window and the session's record
//!   count are all capped, so a model that never stops asking is stopped rather than
//!   tolerated.
//!
//! # What this deliberately does not do
//!
//! * **No process execution and no sandboxing.** Tools run in-process, as they already did.
//!   A tool that spawns a subprocess needs an enforcement boundary the tool cannot reach
//!   around, and that belongs with the tool's execution environment, not here.
//! * **No summarisation or memory.** When a window overflows, the budget elides and evicts,
//!   deterministically; replacing turns with a model-written summary is a fallible, costly
//!   operation with its own injection surface, and belongs in a component above this one that
//!   reads records through the store and appends the summary back as an ordinary pinned
//!   record.
//! * **No token-level streaming.** [`Agent::stream`](aik_api::agent::Agent::stream) reports
//!   at action granularity — content, a tool call, its result, the response — using
//!   [`ModelProvider::complete`](aik_api::model::ModelProvider::complete), because that is
//!   what every provider supports. Driving
//!   [`ModelProvider::stream`](aik_api::model::ModelProvider::stream) means assembling
//!   partial tool calls from chunks, which is a provider-shaped problem worth solving once
//!   there is a frontend to measure it against.
//! * **No planning, retries or model routing.** One model, one loop, one shot per turn.

mod agent;
mod component;
mod run;
mod settings;

pub use agent::{AGENT_ATTRIBUTE, AgentLoop, SESSION_ATTRIBUTE};
pub use component::{AgentComponent, DEFAULT_COMPONENT_ID};
pub use settings::{
    AgentLoopSettings, DEFAULT_MAX_PART_TOKENS, DEFAULT_MAX_TOOL_CALLS, DEFAULT_MAX_TURNS,
    DEFAULT_MAX_WINDOW_TOKENS,
};
