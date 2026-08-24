//! Assembling a kernel from resolved [`RuntimeSettings`].
//!
//! This module registers components and nothing else. Every decision it makes is a *wiring*
//! decision — which tools exist, which policy engine reads which configuration, where
//! approvals go, whether anything is written to a disk — and none of them is an
//! authorization decision. A frontend cannot allow anything: it can only decline to
//! register a capability, which narrows what the rest of the system will consider and can
//! never widen it.
//!
//! # Why this is not in a frontend
//!
//! It used to be, when there was one. There are now two — a terminal that comes and goes, and
//! a host process that stays — and the moment there were two, "how the system is put
//! together" stopped being a property of either. Two copies of this function would be two
//! deployments that agree today: the same policy section, the same component ids, the same
//! four durable subsystems over the same database. They would not stay that way, and the
//! first divergence anybody noticed would be a capability present in one and absent in the
//! other, which is a security difference.
//!
//! So there is one, here, below both frontends and above every subsystem. The dependency
//! direction is what keeps it honest: this crate depends on every implementation crate, and
//! nothing it depends on depends back.
//!
//! # The durable stack
//!
//! The transcript, the agent's memories, the schedule and the audit trail are four subsystems
//! over one database, and each publishes the same capability under the same component id
//! whichever backend is chosen. So the choice is one `match` on [`Storage`] and nothing
//! downstream changes: the agent resolves `dyn ContextStore`, the memory tools bind to
//! `dyn MemoryStore`, and neither can tell — nor should be able to tell — whether what it
//! got survives a restart.
//!
//! The audit trail is registered unconditionally, in both arms, and there is no switch that
//! turns it off. Every other capability here can be narrowed — a tool left unregistered, a
//! memory mode reduced — because narrowing is what a frontend is allowed to do. Declining to
//! record what the system was allowed to do is not narrowing: it is removing the account of
//! everything that *was* allowed, which is the opposite of a safe default. An ephemeral
//! deployment gets the in-memory trail, so it is still auditable while it runs without
//! anything reaching a disk it promised not to touch.
//!
//! The database itself is opened by [`StoreComponent`], which reads its path from the
//! configuration tree. A frontend resolves that path into
//! [`RuntimeSettings::storage`] and pins it there with
//! [`pin_database_path`](crate::settings::pin_database_path), so the component never consults
//! the process environment on its own and the path a frontend reports is the path on disk.
//!
//! # Exactly one process may hold the database
//!
//! redb takes an exclusive lock on the file. That is not a limitation to work around — it is
//! what makes a write spanning two subsystems' tables one transaction — but it does mean the
//! second process to assemble a persistent deployment fails to start. Which is the whole
//! reason [`aik-daemon`](../aik_daemon/index.html) exists: one host process owns the database
//! and the kernel over it, and everything else talks to that process.

use std::sync::Arc;

use aik_agent::AgentComponent;
use aik_api::model::{ModelId, ModelProvider};
use aik_api::permission::ApprovalSink;
use aik_approval::{ApprovalBroker, ApprovalComponent};
use aik_audit::{AuditComponent, RedbAuditComponent};
use aik_context::{ContextComponent, RedbContextComponent};
use aik_core::prelude::*;
use aik_fs::{FsListTool, FsReadTool, FsWriteTool};
use aik_memory::{MemoryComponent, MemoryToolsComponent, RedbMemoryComponent};
use aik_ollama::OllamaComponent;
use aik_policy::RuleBasedPolicyEngine;
use aik_scheduler::{RedbSchedulerComponent, SchedulerComponent};
use aik_store::StoreComponent;
use aik_tools::ToolsComponent;

use crate::jobs::AgentJobComponent;
use crate::settings::{JobExecution, MemorySet, RuntimeSettings, Storage, ToolSet};

/// The configuration path the policy document is read from.
pub use crate::settings::POLICY_SECTION;

/// Everything a frontend needs to hold onto after the kernel is built.
#[derive(Debug)]
pub struct Assembled {
    /// The kernel, not yet started.
    pub kernel: Kernel,
    /// The broker approvals are parked on.
    ///
    /// Held by the frontend rather than resolved later because answering requires an
    /// [`ApprovalGate`](aik_approval::ApprovalGate), and whether one is ever attached is
    /// what separates a session somebody is watching from one nobody is.
    pub broker: Arc<ApprovalBroker>,
}

/// Builds every component the deployment owns *except* the model provider.
///
/// Split out so the same wiring can be started against a stub provider in tests: the model
/// is the one collaborator that needs a server, and everything worth testing about this
/// function is what it does with the others.
pub fn builder(
    settings: &RuntimeSettings,
    model: ModelId,
) -> Result<(KernelBuilder, Arc<ApprovalBroker>)> {
    let broker = Arc::new(ApprovalBroker::new());

    // Read and validated here so a malformed document fails at startup, naming the rule that
    // is wrong, rather than at the first tool call.
    let policy = RuleBasedPolicyEngine::from_config(&settings.config, POLICY_SECTION)?;

    let mut tools = ToolsComponent::new()
        .with_policy(Arc::new(policy))
        .with_approvals(broker.clone() as Arc<dyn ApprovalSink>);

    // A tool that is not registered cannot be reached however permissive the policy is, so
    // this is the outer limit and policy is the inner one. Both apply; neither substitutes
    // for the other.
    match settings.tools {
        ToolSet::None => {}
        ToolSet::ReadOnly => {
            tools = tools
                .with_tool(FsReadTool::new(&settings.root)?)
                .with_tool(FsListTool::new(&settings.root)?);
        }
        ToolSet::ReadWrite => {
            tools = tools
                .with_tool(FsReadTool::new(&settings.root)?)
                .with_tool(FsListTool::new(&settings.root)?)
                .with_tool(FsWriteTool::new(&settings.root)?);
        }
    }

    // The same limit again, over the record store. The tools are handed out by a component
    // that binds them to whichever `dyn MemoryStore` the kernel published, so the volatile
    // and the durable backend are wired identically and the frontend never has to know
    // which one it got.
    let memory_tools = MemoryToolsComponent::new();
    match settings.memory {
        MemorySet::Off => {}
        MemorySet::Recall => {
            tools = tools
                .with_tool(memory_tools.get())
                .with_tool(memory_tools.query());
        }
        MemorySet::Remember => {
            tools = tools
                .with_tool(memory_tools.get())
                .with_tool(memory_tools.query())
                .with_tool(memory_tools.put());
        }
        MemorySet::Full => {
            tools = tools
                .with_tool(memory_tools.get())
                .with_tool(memory_tools.query())
                .with_tool(memory_tools.put())
                .with_tool(memory_tools.delete());
        }
    }

    let agent = AgentComponent::new(settings.agent.clone(), settings.loop_settings(model))
        .described("the assistant this deployment serves")
        .requires(aik_tools::DEFAULT_COMPONENT_ID)
        .requires(aik_context::DEFAULT_COMPONENT_ID)
        .requires(settings.model_component.clone());

    let builder = Kernel::builder()
        .config(settings.config.clone())
        .component(ApprovalComponent::new(broker.clone()))
        .component(tools);

    // One decision, applied to all four: see [`Storage`]. Both arms publish the same
    // capabilities under the same component ids, so nothing downstream — the agent, the
    // memory tools, the registry — can tell which one it got except by outliving a restart.
    let builder = match &settings.storage {
        Storage::Ephemeral => builder
            .component(ContextComponent::new())
            .component(MemoryComponent::new())
            .component(SchedulerComponent::new())
            .component(AuditComponent::new()),
        Storage::Persistent(_) => builder
            .component(StoreComponent::new())
            .component(RedbContextComponent::new())
            .component(RedbMemoryComponent::new())
            .component(RedbSchedulerComponent::new())
            .component(RedbAuditComponent::new()),
    };

    // Registered only when it has something to bind. With no memory tools exposed, the
    // store is still there — it is infrastructure, and something other than a tool may come
    // to use it — but nothing hands a model a door onto it.
    let builder = match settings.memory {
        MemorySet::Off => builder,
        _ => builder.component(memory_tools),
    };

    // The schedule is wired either way; whether anything in *this* process runs what it
    // holds is a separate decision. See [`JobExecution`].
    let builder = match settings.jobs {
        JobExecution::Disabled => builder,
        JobExecution::Agent => builder.component(AgentJobComponent::new()),
    };

    Ok((builder.component(agent), broker))
}

/// Builds the deployment's kernel, with the Ollama provider as its model source.
pub fn assemble(settings: &RuntimeSettings, model: ModelId) -> Result<Assembled> {
    let (builder, broker) = builder(settings, model)?;
    let kernel = builder.component(OllamaComponent::new()).build()?;
    Ok(Assembled { kernel, broker })
}

/// Starts a throwaway kernel holding only the model provider, to ask it what it serves.
///
/// Needed because the model has to be chosen *before* the real kernel is built — the agent
/// component takes it as fixed settings, deliberately, so that nothing during a run can
/// change which model answers.
pub async fn first_available_model(settings: &RuntimeSettings) -> Result<ModelId> {
    let kernel = Kernel::builder()
        .config(settings.config.clone())
        .component(OllamaComponent::new())
        .build()?;
    kernel.start().await?;

    let found = async {
        let provider = kernel.context().service::<dyn ModelProvider>()?;
        provider
            .models()
            .await?
            .into_iter()
            .next()
            .map(|descriptor| descriptor.id)
            .ok_or_else(|| Error::other("the model provider reports no models"))
    }
    .await;

    kernel.shutdown().await?;
    found
}
