//! System assembly for the AI kernel.
//!
//! Every crate below this one is a capability: a provider that answers, a registry that
//! authorizes, tools that act, a broker that asks, stores that remember, a loop that ties
//! them together. None of them decides *which* of those a running system has. That decision
//! is this crate, and deliberately only that.
//!
//! ```text
//!   configuration ──▶ settings::Deployment ──▶ RuntimeSettings
//!
//!   RuntimeSettings ──▶ wiring::builder ──▶ KernelBuilder ──▶ Kernel
//!         │                                                     │
//!         │                                                     ▼
//!         └──▶ principal()  ──── ExecutionContext ───▶ dyn Agent, dyn ContextStore,
//!                                                      dyn Scheduler, dyn AuditStore
//! ```
//!
//! # Who this is for
//!
//! Two frontends, and any third that comes later:
//!
//! * [`aik-cli`](../aik_cli/index.html) — a terminal, one conversation, ending when the
//!   person walks away.
//! * [`aik-daemon`](../aik_daemon/index.html) — a host process that owns the database, runs
//!   the schedule, and serves clients over a local socket.
//!
//! Both assemble the *same* system from the same function, out of settings resolved by the
//! same function: [`settings::Deployment`] reads every deployment-wide value —
//! [`AGENT_SECTION`], the policy document, the database path — so that neither frontend
//! interprets any of it on its own. What differs is what each of them adds on top (a socket,
//! a verbosity, a connection limit) and what they do with the kernel afterwards, which is
//! exactly the difference that should be visible in their code and nowhere else.
//!
//! # What this crate is not allowed to be
//!
//! * **It never authorizes.** There is no policy evaluation here and no `Decision`. It
//!   registers the policy engine and hands it the configured document; what that document
//!   means is [`aik_policy`]'s business.
//! * **It only ever narrows.** Choosing not to register the write tool, or any tool, removes
//!   a capability. There is no switch here that adds one, and the audit trail cannot be
//!   turned off at all.
//! * **It never impersonates.** [`RuntimeSettings::principal`] is built once from resolved
//!   settings and is the agent acting *for* the user, never the user. Nothing downstream can
//!   widen it, and a model has no way to influence it.
//!
//! ```no_run
//! use aik_api::model::ModelId;
//! use aik_runtime::{RuntimeSettings, ToolSet, wiring};
//!
//! # async fn build() -> aik_core::Result<()> {
//! let mut settings = RuntimeSettings::new("/srv/workspace");
//! settings.tools = ToolSet::ReadOnly;
//!
//! let assembled = wiring::assemble(&settings, ModelId::new("llama3"))?;
//! assembled.kernel.start().await?;
//! # Ok(())
//! # }
//! ```

pub mod jobs;
pub mod settings;
pub mod wiring;

pub use jobs::{AgentJobComponent, AgentJobHandler, AgentJobPayload};
pub use settings::{
    AGENT_SECTION, DATABASE_PATH_KEY, DEFAULT_AGENT, DEFAULT_USER, Deployment, ENV_PREFIX, ExecSet,
    ExecSettings, JobExecution, MemorySet, POLICY_SECTION, Provider, RuntimeSettings,
    SYSTEM_PROMPT_KEY, Storage, StorageChoice, ToolSet, load_config, pin_database_path,
    system_prompt,
};
pub use wiring::{Assembled, assemble, builder, first_available_model};
