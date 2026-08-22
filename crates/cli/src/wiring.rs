//! Assembling a kernel from resolved [`Settings`].
//!
//! This module registers components and nothing else. Every decision it makes is a *wiring*
//! decision — which tools exist, which policy engine reads which configuration, where
//! approvals go, whether anything is written to a disk — and none of them is an
//! authorization decision. The frontend cannot allow anything: it can only decline to
//! register a capability, which narrows what the rest of the system will consider and can
//! never widen it.
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
//! everything that *was* allowed, which is the opposite of a safe default. An `--ephemeral`
//! run gets the in-memory trail, so the run is still auditable while it happens without
//! anything reaching a disk that run promised not to touch.
//!
//! The database itself is opened by [`StoreComponent`], which reads its path from the
//! configuration tree. The frontend resolves that path in
//! [`Settings`](crate::settings::Settings) and pins it there, so the component never
//! consults the process environment on its own and the path in the banner is the path on
//! disk.

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

use crate::args::{MemorySet, ToolSet};
use crate::settings::{Settings, Storage};

/// The configuration path the policy document is read from.
pub const POLICY_SECTION: &str = "policy";

/// Everything the frontend needs to hold onto after the kernel is built.
#[derive(Debug)]
pub struct Assembled {
    /// The kernel, not yet started.
    pub kernel: Kernel,
    /// The broker approvals are parked on.
    ///
    /// Held by the frontend rather than resolved later because answering requires an
    /// [`ApprovalGate`](aik_approval::ApprovalGate), and whether one is ever attached is
    /// what separates an interactive session from a one-shot run.
    pub broker: Arc<ApprovalBroker>,
}

/// Builds every component the frontend owns *except* the model provider.
///
/// Split out so the same wiring can be started against a stub provider in tests: the model
/// is the one collaborator that needs a server, and everything worth testing about this
/// function is what it does with the other five.
pub fn builder(
    settings: &Settings,
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
        .described("the terminal frontend's assistant")
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

    Ok((builder.component(agent), broker))
}

/// Builds the frontend's kernel, with the Ollama provider as its model source.
pub fn assemble(settings: &Settings, model: ModelId) -> Result<Assembled> {
    let (builder, broker) = builder(settings, model)?;
    let kernel = builder.component(OllamaComponent::new()).build()?;
    Ok(Assembled { kernel, broker })
}

/// Starts a throwaway kernel holding only the model provider, to ask it what it serves.
///
/// Needed because the model has to be chosen *before* the real kernel is built — the agent
/// component takes it as fixed settings, deliberately, so that nothing during a run can
/// change which model answers.
pub async fn first_available_model(settings: &Settings) -> Result<ModelId> {
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
