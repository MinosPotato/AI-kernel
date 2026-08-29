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
//! five durable subsystems over the same database. They would not stay that way, and the
//! first divergence anybody noticed would be a capability present in one and absent in the
//! other, which is a security difference.
//!
//! So there is one, here, below both frontends and above every subsystem. The dependency
//! direction is what keeps it honest: this crate depends on every implementation crate, and
//! nothing it depends on depends back.
//!
//! # The durable stack
//!
//! The transcript, the agent's memories, the schedule, the audit trail and the spend ledger
//! are five subsystems over one database, and each publishes the same capability under the
//! same component id
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
use aik_anthropic::AnthropicComponent;
use aik_api::model::{ModelId, ModelProvider};
use aik_api::permission::ApprovalSink;
use aik_api::tool::ToolCatalog;
use aik_approval::{ApprovalBroker, ApprovalComponent};
use aik_audit::{AuditComponent, RedbAuditComponent};
use aik_context::{ContextComponent, RedbContextComponent};
use aik_core::prelude::*;
use aik_exec::{ExecTool, Sandbox};
use aik_fs::{FsListTool, FsReadTool, FsWriteTool};
use aik_mcp::{McpCatalog, McpClient, McpComponent};
use aik_memory::{MemoryComponent, MemoryToolsComponent, RedbMemoryComponent};
use aik_ollama::OllamaComponent;
use aik_openai::OpenAiComponent;
use aik_policy::RuleBasedPolicyEngine;
use aik_quota::{QuotaComponent, QuotaDocument, RedbQuotaComponent};
use aik_resilience::ResilienceComponent;
use aik_scheduler::{RedbSchedulerComponent, SchedulerComponent};
use aik_store::StoreComponent;
use aik_summary::SummaryComponent;
use aik_tools::ToolsComponent;

use crate::jobs::AgentJobComponent;
use crate::schedule_tools::ScheduleToolsComponent;
use crate::settings::{
    ExecSet, ExecSettings, JobExecution, MCP_SERVERS_PATH, MemorySet, Provider, RuntimeSettings,
    Storage, ToolSet,
};

/// The configuration path the policy document is read from.
pub use crate::settings::POLICY_SECTION;
/// The configuration path the spend ceilings are read from.
pub use crate::settings::QUOTA_SECTION;

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

    // The other half of "what may this deployment do": policy decides whether, this decides
    // how much. Read at the same point and for the same reason — a ceiling nobody can parse
    // should stop the process, not the first turn.
    let quota = QuotaDocument::from_config(&settings.config, QUOTA_SECTION)?;

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

    // Registering this is not like registering the others: it does not grant a capability
    // whose limits are written here, it starts host code whose limits are whatever the
    // allowlisted programs happen to have. So it is off unless a deployment turned it on, and
    // turning it on with a sandbox is verified *now* — a host that cannot provide one fails to
    // start rather than running programs unconfined and saying nothing.
    if settings.exec.is_enabled() {
        tools = tools.with_tool(exec_tool(settings)?);
    }

    // The same shape again, over tools this repository did not write. Every server's
    // settings are checked here, at startup, so a malformed one names itself instead of
    // surfacing as a tool that is mysteriously absent; the servers themselves are started
    // lazily, on the first listing, because a host that is having a bad day should not stop
    // a kernel that has other work.
    let mcp = mcp_catalog(settings)?;
    if let Some(catalog) = &mcp {
        tools = tools.with_catalog(catalog.clone() as Arc<dyn ToolCatalog>);
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

    // Scheduling a prompt is only offered when something in this process will actually run
    // it: with `JobExecution::Disabled`, `schedule.create` would let a model create a job
    // that can never fire, since no `AgentJobHandler` is registered to be its handler.
    let schedule_tools = ScheduleToolsComponent::new();
    if matches!(settings.jobs, JobExecution::Agent) {
        tools = tools
            .with_tool(schedule_tools.create())
            .with_tool(schedule_tools.list())
            .with_tool(schedule_tools.cancel());
    }

    let mut agent = AgentComponent::new(
        settings.agent.clone(),
        settings.loop_settings(model.clone()),
    )
    .described("the assistant this deployment serves")
    .requires(aik_tools::DEFAULT_COMPONENT_ID)
    .requires(aik_context::DEFAULT_COMPONENT_ID)
    // Declared unconditionally, and the guard is registered unconditionally to match. A
    // deployment that configured ceilings and silently did not get them is the one outcome
    // worth designing against, and the way to make it impossible is to leave nothing
    // conditional: an empty document produces a guard that refuses nothing and writes
    // nothing, which is exactly what "no quota" meant before there was one.
    .requires(aik_quota::DEFAULT_COMPONENT_ID)
    // The resilience layer, not the provider underneath it. Both would resolve the same
    // capability, but only this ordering guarantees the agent gets the wrapped one: the
    // wrapper becomes the registry's default during its own `init`, so a component that
    // initialised before it would hold the bare provider for the rest of the process.
    .requires(aik_resilience::DEFAULT_COMPONENT_ID);

    // Registered before the agent and declared as a dependency of it, because the agent
    // resolves `dyn ContextCompactor` optionally during `init`: a compactor initialised
    // afterwards would be a deployment that configured compaction and silently did not get
    // it. Leaving it out is the supported way to have none — the loop then evicts
    // deterministically, exactly as it did before this existed.
    let compactor = settings.summary.is_enabled().then(|| {
        SummaryComponent::new(settings.summary.resolve(model))
            .requires(aik_context::DEFAULT_COMPONENT_ID)
            // The wrapper, for the same reason the agent depends on it: a compaction is a
            // model call like any other, and one that gave up on the first 503 would cost a
            // run its whole transcript at exactly the moment the transcript is largest.
            .requires(aik_resilience::DEFAULT_COMPONENT_ID)
    });
    if compactor.is_some() {
        agent = agent.requires(aik_summary::DEFAULT_COMPONENT_ID);
    }

    let builder = Kernel::builder()
        .config(settings.config.clone())
        .component(ApprovalComponent::new(broker.clone()))
        // Registered unconditionally, like the audit trail and the quota guard, and for the
        // same reason: the failure worth designing against is a deployment that believed it
        // had this and did not. Its settings can narrow it to a pass-through — one attempt,
        // no breaker, no concurrency limit — but nothing removes it from the wiring, so the
        // question "does this deployment retry?" is answered by one configuration section
        // rather than by whether a component happens to be present.
        .component(ResilienceComponent::from_config().wrapping(settings.model_component.clone()))
        .component(tools);

    // Registered for its shutdown, not for anything it provides: a tool server is a process
    // this kernel started, and a kernel that exited leaving one running would be a kernel
    // whose shutdown is not one. The catalogue itself is passed to the registry directly, for
    // the same reason the policy engine is — component start-up order must not be what
    // decides whether a deployment has these tools.
    let builder = match &mcp {
        Some(catalog) => builder.component(McpComponent::new(catalog.clone())),
        None => builder,
    };

    // Whether the record store ranks by meaning, and through which component. Resolved
    // before either storage arm because it applies to both: the two backends differ in what
    // outlives a restart and in nothing else, and a semantic search that worked on only one
    // of them would be exactly the drift they are kept identical to avoid.
    let embedder = embedder_choice(settings)?;

    // One decision, applied to all four: see [`Storage`]. Both arms publish the same
    // capabilities under the same component ids, so nothing downstream — the agent, the
    // memory tools, the registry — can tell which one it got except by outliving a restart.
    let builder = match &settings.storage {
        Storage::Ephemeral => {
            let memory = MemoryComponent::new();
            let memory = match &embedder {
                Some((component, model)) => memory.with_embedder(component.clone(), model.clone()),
                None => memory,
            };
            builder
                .component(ContextComponent::new())
                .component(memory)
                .component(SchedulerComponent::new())
                .component(AuditComponent::new())
                .component(QuotaComponent::new(quota))
        }
        Storage::Persistent(_) => {
            let memory = RedbMemoryComponent::new();
            let memory = match &embedder {
                Some((component, model)) => memory.with_embedder(component.clone(), model.clone()),
                None => memory,
            };
            builder
                .component(StoreComponent::new())
                .component(RedbContextComponent::new())
                .component(memory)
                .component(RedbSchedulerComponent::new())
                .component(RedbAuditComponent::new())
                .component(RedbQuotaComponent::new(quota))
        }
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
        JobExecution::Agent => builder
            .component(AgentJobComponent::new())
            .component(schedule_tools),
    };

    let builder = match compactor {
        Some(compactor) => builder.component(compactor),
        None => builder,
    };

    Ok((builder.component(agent), broker))
}

/// Which component embeds, and with which model, or `None` for a store that does not rank
/// by meaning.
///
/// Refuses rather than degrades in the one case where the two settings disagree: a
/// deployment that named an embedding model but chose a provider with no embeddings endpoint
/// has written down something the system cannot do, and starting anyway would give it a
/// memory that silently never searches. The message names the provider that does have one,
/// because that is the actual fix.
fn embedder_choice(settings: &RuntimeSettings) -> Result<Option<(ComponentId, ModelId)>> {
    let Some(model) = &settings.embedding_model else {
        return Ok(None);
    };
    let component = settings.provider.embedder_component_id().ok_or_else(|| {
        Error::config(
            "agent.embedding_model",
            format!(
                "provider `{}` serves no embeddings, so `{model}` cannot be used to search \
                 memories by meaning; either clear `agent.embedding_model` or use the ollama \
                 or openai provider, both of which do",
                settings.provider.as_str()
            ),
        )
    })?;
    Ok(Some((component, model.clone())))
}

/// Builds the catalogue of external tool servers this deployment asked for, if any.
///
/// `None` covers both ways of having none — a run that did not ask, and a deployment that
/// describes no servers — because they produce the same system and neither is a failure. A
/// run that *did* ask and found nothing described is the one case that is: it is a command
/// line that says the agent has external tools and a configuration that gives it none, and
/// the operator should hear about that at startup rather than from a model that cannot do
/// what they expected.
fn mcp_catalog(settings: &RuntimeSettings) -> Result<Option<Arc<McpCatalog>>> {
    if !settings.mcp.is_enabled() {
        return Ok(None);
    }

    if settings.mcp_settings.servers.is_empty() {
        return Err(Error::config(
            MCP_SERVERS_PATH,
            "external tool servers were enabled but none are described; name the servers this \
             deployment runs, or leave them off",
        ));
    }

    let clients = settings
        .mcp_settings
        .servers
        .iter()
        .map(|server| {
            server
                .resolve_at(&settings.root, MCP_SERVERS_PATH)
                .map(|resolved| Arc::new(McpClient::new(resolved)))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(Arc::new(McpCatalog::new(clients)?)))
}

/// Builds the process-execution tool this deployment asked for.
///
/// Every refusal here is a startup failure naming the setting that caused it, because each of
/// them is a deployment that would otherwise look enabled and be either useless or unsafe: an
/// allowlist nobody filled in, a root that does not exist, a sandbox the host cannot provide.
fn exec_tool(settings: &RuntimeSettings) -> Result<ExecTool> {
    let ExecSettings {
        programs,
        writable,
        network,
        timeout_ms,
        search_path,
    } = &settings.exec_settings;

    if programs.is_empty() {
        return Err(Error::config(
            format!("{}.exec.programs", crate::settings::AGENT_SECTION),
            "process execution was enabled but no programs are allowed; name the programs the \
             agent may run, or leave execution off",
        ));
    }

    let sandbox = match settings.exec {
        ExecSet::Sandboxed => Sandbox::bubblewrap()?,
        ExecSet::Unconfined => Sandbox::unconfined(),
        // Unreachable: the caller checks `is_enabled` first. Answered rather than panicked on,
        // because a wiring function is the wrong place to be certain about a caller.
        ExecSet::Off => {
            return Err(Error::config(
                format!("{}.exec", crate::settings::AGENT_SECTION),
                "process execution is off",
            ));
        }
    };

    // The confinement root is the deployment's, not a second one: a workspace a program could
    // write that the filesystem tools could not read would be two different ideas of where
    // this agent works.
    let mut tool = ExecTool::new(&settings.root, sandbox, programs.iter())?
        .writable(*writable)
        .with_network(*network);
    if let Some(timeout) = timeout_ms {
        tool = tool.with_timeout(std::time::Duration::from_millis(*timeout));
    }
    if let Some(path) = search_path {
        tool = tool.with_search_path(path.clone());
    }
    Ok(tool)
}

/// Builds the deployment's kernel, with the configured provider as its model source.
pub fn assemble(settings: &RuntimeSettings, model: ModelId) -> Result<Assembled> {
    let (builder, broker) = builder(settings, model)?;
    let kernel = provider_component(builder, settings.provider).build()?;
    Ok(Assembled { kernel, broker })
}

/// Registers the one provider this deployment chose.
///
/// One provider, not both. Registering the second as a non-default would leave a kernel
/// holding a credential and an outbound connection nothing was configured to use, and a
/// deployment that meant to keep every conversation on this machine would be one
/// `service_named` call away from not doing that.
fn provider_component(builder: KernelBuilder, provider: Provider) -> KernelBuilder {
    match provider {
        Provider::Ollama => builder.component(OllamaComponent::new()),
        Provider::Anthropic => builder.component(AnthropicComponent::new()),
        Provider::OpenAi => builder.component(OpenAiComponent::new()),
    }
}

/// Starts a throwaway kernel holding only the model provider, to ask it what it serves.
///
/// Needed because the model has to be chosen *before* the real kernel is built — the agent
/// component takes it as fixed settings, deliberately, so that nothing during a run can
/// change which model answers.
pub async fn first_available_model(settings: &RuntimeSettings) -> Result<ModelId> {
    let kernel = provider_component(
        Kernel::builder().config(settings.config.clone()),
        settings.provider,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ExecSet;
    use aik_core::ErrorKind;

    /// Settings rooted at a real directory, since the tool canonicalises its workspace.
    fn settings(directory: &std::path::Path, exec: ExecSet, programs: &[&str]) -> RuntimeSettings {
        let mut settings = RuntimeSettings::new(directory);
        settings.exec = exec;
        settings.exec_settings = ExecSettings {
            programs: programs.iter().map(|name| (*name).to_owned()).collect(),
            ..ExecSettings::default()
        };
        settings
    }

    #[test]
    fn execution_enabled_with_no_programs_fails_at_startup() {
        // The failure mode this rules out is a deployment that looks enabled, registers a tool
        // the model is told about, and refuses every call it makes.
        let directory = tempfile::tempdir().unwrap();
        let error = exec_tool(&settings(directory.path(), ExecSet::Unconfined, &[])).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(
            format!("{error}").contains("agent.exec.programs"),
            "{error}"
        );
    }

    #[test]
    fn the_tool_works_in_the_deployment_s_own_confinement_root() {
        // Not a second directory: a program that could write where the filesystem tools cannot
        // read would be two different ideas of where this agent works.
        let directory = tempfile::tempdir().unwrap();
        let tool = exec_tool(&settings(directory.path(), ExecSet::Unconfined, &["git"])).unwrap();

        assert_eq!(tool.workspace(), directory.path().canonicalize().unwrap());
        assert_eq!(tool.programs().collect::<Vec<_>>(), ["git"]);
        assert!(!tool.sandbox().is_enforcing());
    }

    #[test]
    fn an_allowlist_entry_that_is_not_a_program_name_fails_at_startup() {
        let directory = tempfile::tempdir().unwrap();
        let error = exec_tool(&settings(
            directory.path(),
            ExecSet::Unconfined,
            &["/bin/sh"],
        ))
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn a_deployment_that_runs_nothing_never_reaches_the_tool_at_all() {
        let directory = tempfile::tempdir().unwrap();
        let off = settings(directory.path(), ExecSet::Off, &["git"]);

        assert!(!off.exec.is_enabled());
        // And asking for it anyway is answered rather than panicked on.
        assert_eq!(exec_tool(&off).unwrap_err().kind(), ErrorKind::Config);
    }

    /// Stands in for the provider component the agent and the compactor depend on by name.
    ///
    /// Registered but never started, because these tests are about which components a
    /// deployment *has*: dependency resolution happens at build time and needs the id to
    /// exist, while resolving `dyn ModelProvider` happens at `init` and does not.
    struct StubProvider;

    #[async_trait]
    impl Component for StubProvider {
        fn descriptor(&self) -> ComponentDescriptor {
            ComponentDescriptor::new(ComponentId::new("model.stub"))
        }
    }

    /// The component ids a deployment's builder registers, without starting anything.
    fn registered(settings: &RuntimeSettings) -> Vec<ComponentId> {
        let (builder, _) = builder(settings, ModelId::new("test-model")).expect("wiring");
        builder
            .component(StubProvider)
            .build()
            .expect("a kernel")
            .component_ids()
            .into_iter()
            .collect()
    }

    #[test]
    fn a_deployment_compacts_long_sessions_unless_it_says_otherwise() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        // The agent depends on the provider component by name, so point both at something
        // this test does not have to start.
        settings.model_component = ComponentId::new("model.stub");

        let ids = registered(&settings);
        assert!(
            ids.contains(&ComponentId::new(aik_summary::DEFAULT_COMPONENT_ID)),
            "{ids:?}"
        );
    }

    #[test]
    fn compaction_turned_off_registers_nothing_and_the_agent_stops_depending_on_it() {
        // The failure this rules out is an agent left declaring a dependency on a component
        // nobody registered, which is a kernel that does not start at all.
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.model_component = ComponentId::new("model.stub");
        settings.summary.enabled = Some(false);

        let ids = registered(&settings);
        assert!(
            !ids.contains(&ComponentId::new(aik_summary::DEFAULT_COMPONENT_ID)),
            "{ids:?}"
        );
    }

    #[test]
    fn a_run_that_did_not_ask_for_external_tools_starts_no_server() {
        let directory = tempfile::tempdir().unwrap();
        let settings = RuntimeSettings::new(directory.path());
        assert!(mcp_catalog(&settings).unwrap().is_none());
    }

    #[test]
    fn asking_for_external_tools_with_none_described_fails_at_startup() {
        // The failure this rules out is a command line that says the agent has external
        // tools and a configuration that gives it none.
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.mcp = crate::settings::McpSet::On;

        let error = mcp_catalog(&settings).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(format!("{error}").contains(MCP_SERVERS_PATH), "{error}");
    }

    #[test]
    fn a_malformed_server_names_the_setting_that_is_wrong() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.mcp = crate::settings::McpSet::On;
        settings.mcp_settings.servers = vec![aik_mcp::ServerSettings {
            label: "files".into(),
            command: "/usr/bin/server".into(),
            ..aik_mcp::ServerSettings::default()
        }];

        let error = mcp_catalog(&settings).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(
            format!("{error}").contains("agent.mcp.servers[files].command"),
            "{error}"
        );
    }

    #[test]
    fn a_described_server_is_wired_but_not_started() {
        // Building the catalogue must not touch the host: a kernel that spawned every tool
        // server before it finished assembling would fail to start over a program that is
        // temporarily missing.
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.model_component = ComponentId::new("model.stub");
        settings.mcp = crate::settings::McpSet::On;
        settings.mcp_settings.servers = vec![aik_mcp::ServerSettings {
            label: "files".into(),
            command: "definitely-not-installed".into(),
            ..aik_mcp::ServerSettings::default()
        }];

        let ids = registered(&settings);
        assert!(
            ids.contains(&ComponentId::new(aik_mcp::DEFAULT_COMPONENT_ID)),
            "{ids:?}"
        );
    }

    #[test]
    fn no_embedding_model_means_no_embedder_and_no_new_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let settings = RuntimeSettings::new(directory.path());
        assert!(embedder_choice(&settings).unwrap().is_none());
    }

    #[test]
    fn an_embedding_model_points_the_store_at_the_providers_own_component() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.provider = Provider::Ollama;
        settings.embedding_model = Some(ModelId::new("nomic-embed-text"));

        let (component, model) = embedder_choice(&settings).unwrap().expect("a choice");
        assert_eq!(component, Provider::Ollama.component_id());
        assert_eq!(model, ModelId::new("nomic-embed-text"));
    }

    #[test]
    fn asking_a_provider_with_no_embeddings_to_embed_fails_at_startup() {
        // The failure mode this rules out is a deployment that configured semantic memory,
        // started fine, and searched nothing for the rest of its life.
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.provider = Provider::Anthropic;
        settings.model_component = Provider::Anthropic.component_id();
        settings.embedding_model = Some(ModelId::new("nomic-embed-text"));

        let error = embedder_choice(&settings).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(format!("{error}").contains("ollama"), "{error}");
    }

    #[test]
    fn a_hosted_provider_that_does_embed_points_the_store_at_its_own_component() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.provider = Provider::OpenAi;
        settings.model_component = Provider::OpenAi.component_id();
        settings.embedding_model = Some(ModelId::new("text-embedding-3-small"));

        let (component, model) = embedder_choice(&settings).unwrap().expect("an embedder");
        assert_eq!(component, Provider::OpenAi.component_id());
        assert_eq!(model, ModelId::new("text-embedding-3-small"));
    }

    /// The two backends must be indistinguishable here too: whichever one a deployment gets,
    /// a configured embedding model has to reach it.
    #[test]
    fn both_storage_backends_carry_the_embedding_model_into_the_kernel() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(directory.path());
        settings.embedding_model = Some(ModelId::new("nomic-embed-text"));

        for storage in [
            Storage::Ephemeral,
            Storage::Persistent(directory.path().join("aik.redb")),
        ] {
            settings.storage = storage;
            // Building is the assertion: the memory component declares the embedder
            // component as a dependency, and the kernel refuses to build when a declared
            // dependency is not registered.
            assemble(&settings, ModelId::new("llama3.2")).expect("the kernel wires up");
        }
    }
}
