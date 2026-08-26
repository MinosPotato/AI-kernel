//! One resolved description of a deployment.
//!
//! [`RuntimeSettings`] is everything [`wiring`](crate::wiring) needs and nothing else: who
//! the agent is, who it acts for, which tools exist, where anything durable is kept. It is
//! deliberately *not* a frontend's settings type. A terminal run also has a prompt, a
//! verbosity, a place to write a measurement file; a daemon also has a socket and a set of
//! connections. Neither of those changes how the system is assembled, so neither belongs
//! here — and keeping them out is what lets both frontends assemble the *same* system rather
//! than two that drift.
//!
//! # What is decided here and what is not
//!
//! Every field is a narrowing. A tool left out of [`ToolSet`] cannot be reached however
//! permissive a policy is; a memory mode below [`MemorySet::Full`] removes a door rather
//! than opening one. Nothing here authorizes anything: the policy document is carried
//! through as configuration and read by
//! [`RuleBasedPolicyEngine`](aik_policy::RuleBasedPolicyEngine), and ownership of sessions,
//! memories and jobs is decided by the stores against the principal this type merely names.

use std::path::{Path, PathBuf};

use aik_agent::AgentLoopSettings;
use aik_api::agent::AgentId;
use aik_api::model::ModelId;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_core::ComponentId;
use aik_core::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

/// Where the shared database's path lives in the configuration tree.
///
/// The store component's own section — `components.<id>`, with the dots in `store.db`
/// nesting — so there is exactly one key for it rather than a frontend-specific alias that
/// could disagree with the one the component actually reads.
pub const DATABASE_PATH_KEY: &str = "components.store.db.path";

/// The configuration path the policy document is read from.
pub const POLICY_SECTION: &str = "policy";

/// The configuration section the agent's own settings are read from.
///
/// Deliberately *not* a frontend's section. What the agent is told before its first turn is a
/// property of the deployment, in the same way the policy document and the database path are:
/// a terminal and a host process serving the same project are the same assistant, and an
/// instruction that reached one of them and not the other would be two different assistants
/// answering from one database. Frontend sections carry what is genuinely a frontend's — a
/// socket, a verbosity, a one-shot prompt — and this is not that.
pub const AGENT_SECTION: &str = "agent";

/// Where the agent's pinned instructions live in the configuration tree.
pub const SYSTEM_PROMPT_KEY: &str = "agent.system_prompt";

/// The agent identity used when nothing configures one.
pub const DEFAULT_AGENT: &str = "assistant";

/// The user identity used when nothing configures one.
///
/// A fixed name rather than the host account, so a policy document written against `user`
/// means the same thing on every machine it is copied to.
pub const DEFAULT_USER: &str = "user";

/// Which model provider a deployment talks to.
///
/// One choice, made once, for the whole deployment — like the model itself and for the same
/// reason: two frontends over one database that resolved different providers would produce
/// one transcript answered by two different services, and the difference would look like the
/// assistant changing its mind rather than like a configuration mistake.
///
/// The default is [`Provider::Ollama`], which is the only one that needs no credential and
/// no network beyond this machine. Choosing [`Provider::Anthropic`] is choosing to send the
/// conversation — and whatever the filesystem tools have read into it — to a third party, so
/// it is never the default and is always something a deployment wrote down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    /// A local or self-hosted Ollama server.
    #[default]
    Ollama,
    /// The Anthropic Messages API, which needs an API key. See
    /// [`aik_anthropic`](../aik_anthropic/index.html) for where that key may and may not
    /// live.
    Anthropic,
}

impl Provider {
    /// Parses a provider name, or explains what the accepted ones are.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "ollama" => Ok(Self::Ollama),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(Error::InvalidArgument(format!(
                "provider takes one of ollama, anthropic; got `{other}`"
            ))),
        }
    }

    /// The provider's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Anthropic => "anthropic",
        }
    }

    /// The component id that publishes this provider's `dyn ModelProvider`.
    pub fn component_id(self) -> ComponentId {
        ComponentId::new(match self {
            Self::Ollama => aik_ollama::DEFAULT_COMPONENT_ID,
            Self::Anthropic => aik_anthropic::DEFAULT_COMPONENT_ID,
        })
    }

    /// The component id that publishes this provider's `dyn Embedder`, where it has one.
    ///
    /// [`Provider::Anthropic`] has none: the Messages API serves completions and nothing
    /// else, so there is no endpoint behind which an [`Embedder`](aik_api::model::Embedder)
    /// could be implemented. A deployment on it can still remember and recall exactly — what
    /// it cannot do is rank by meaning, and
    /// [`assemble`](crate::wiring::assemble) says so rather than quietly leaving the setting
    /// out.
    pub fn embedder_component_id(self) -> Option<ComponentId> {
        match self {
            Self::Ollama => Some(ComponentId::new(aik_ollama::DEFAULT_COMPONENT_ID)),
            Self::Anthropic => None,
        }
    }
}

/// Which filesystem tools a deployment registers.
///
/// A tool that is not registered cannot be reached at all, whatever policy says, so this is
/// the outer of the two limits on what the agent can touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSet {
    /// No tools whatsoever. The agent can only talk.
    None,
    /// Reading and listing, confined to the root.
    #[default]
    ReadOnly,
    /// Reading, listing and writing, confined to the root.
    ReadWrite,
}

/// Which memory tools a deployment registers.
///
/// The same outer limit [`ToolSet`] is for the filesystem, applied to the record store: a
/// memory tool that is not registered cannot be reached however permissive the policy is.
/// The modes are cumulative, and the default is [`MemorySet::Remember`] — an assistant that
/// can recall but never record has nothing to recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemorySet {
    /// No memory tools. The agent cannot reach the record store at all.
    Off,
    /// `memory.get` and `memory.query`: recall only, nothing written and nothing forgotten.
    Recall,
    /// Recall, plus `memory.put`.
    #[default]
    Remember,
    /// Everything, including `memory.delete`.
    ///
    /// Deletion is the one memory operation that destroys evidence: a model that can forget
    /// on its own can erase what it was told to do and the record that it was told. It is
    /// therefore never on by default, and the shipped policy still puts every deletion to a
    /// human. Expiry — a `ttl_seconds` on the record — is the non-destructive way to bound
    /// how long a memory lasts, and needs no tool at all.
    Full,
}

impl MemorySet {
    /// Parses a mode name, or explains what the accepted ones are.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "off" => Ok(Self::Off),
            "recall" => Ok(Self::Recall),
            "remember" => Ok(Self::Remember),
            "full" => Ok(Self::Full),
            other => Err(Error::InvalidArgument(format!(
                "memory takes one of off, recall, remember, full; got `{other}`"
            ))),
        }
    }

    /// The mode's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Recall => "recall",
            Self::Remember => "remember",
            Self::Full => "full",
        }
    }
}

/// Whether a deployment can run programs at all, and behind what.
///
/// The default is [`ExecSet::Off`], and it is the only default that could be right. Every
/// other tool in the workspace carries out a request itself, so registering it grants exactly
/// what its documentation says. This one *starts host code*: registering it grants whatever
/// the allowlisted programs do, which is not a property this crate can know. Turning it on is
/// therefore a decision somebody makes, per deployment, having chosen the programs.
///
/// The two enabled modes are not two strengths of the same thing. [`ExecSet::Sandboxed`] is an
/// enforcement boundary the program cannot reach around; [`ExecSet::Unconfined`] is no boundary
/// at all, with the program allowlist as the entire security argument. See
/// [`Sandbox::Unconfined`](aik_exec::Sandbox::Unconfined).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecSet {
    /// No process execution. The agent cannot start anything.
    #[default]
    Off,
    /// Allowlisted programs, inside a namespace sandbox.
    ///
    /// The sandbox is verified at startup: a host that cannot provide one fails to start
    /// rather than quietly running programs unconfined.
    Sandboxed,
    /// Allowlisted programs, with no sandbox.
    ///
    /// Never reached by omitting configuration. A deployment that selects this is saying that
    /// its allowlist is the boundary, because nothing else will be.
    Unconfined,
}

impl ExecSet {
    /// Parses a mode name, or explains what the accepted ones are.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "off" => Ok(Self::Off),
            "sandboxed" => Ok(Self::Sandboxed),
            "unconfined" => Ok(Self::Unconfined),
            other => Err(Error::InvalidArgument(format!(
                "exec takes one of off, sandboxed, unconfined; got `{other}`"
            ))),
        }
    }

    /// The mode's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Sandboxed => "sandboxed",
            Self::Unconfined => "unconfined",
        }
    }

    /// Whether this mode registers the tool at all.
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// What a deployment says about running programs, read from `agent.exec`.
///
/// Separate from [`ExecSet`] because the two answer different questions and come from
/// different places: the mode is a command-line decision a frontend makes per run, and this is
/// the deployment's standing description of *what* may run — which programs, whether they may
/// write, whether they have a network. A frontend can decline to register the tool; it cannot
/// add a program to this list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecSettings {
    /// The bare program names that may be run. Empty means the tool cannot be registered.
    pub programs: Vec<String>,
    /// Whether programs may write to the confinement root.
    pub writable: bool,
    /// Whether programs have a network.
    pub network: bool,
    /// The per-call wall-clock timeout, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Where programs are looked for, overriding the built-in search path.
    pub search_path: Option<String>,
}

/// Where a deployment writes down its MCP servers.
pub const MCP_SERVERS_PATH: &str = "agent.mcp.servers";

/// Whether external MCP tool servers are reachable in this run.
///
/// A frontend decision, like [`ExecSet`], and for the same reason: *which* servers a
/// deployment has is written in configuration by an operator and cannot be widened from a
/// command line, but whether this particular run starts any of them at all is something the
/// person starting it gets to say. Off is the default, so a run that did not ask for
/// external tools starts no third-party processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpSet {
    /// No server is started and no MCP tool is registered.
    #[default]
    Off,
    /// Every server in `agent.mcp.servers` is available.
    On,
}

impl McpSet {
    /// Reads a mode by name, or explains what the names are.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            other => Err(Error::InvalidArgument(format!(
                "unknown MCP mode `{other}`; expected one of off, on"
            ))),
        }
    }

    /// The name this mode is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    /// Whether this run reaches external tool servers at all.
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

/// What a deployment says about external tool servers, read from `agent.mcp`.
///
/// Separate from [`McpSet`] for the same reason [`ExecSettings`] is separate from
/// [`ExecSet`]: the mode is a per-run decision, and this is the deployment's standing
/// description of *which* servers exist, what they may be given, and what each of their
/// calls may cost. A frontend can decline to start them; it cannot add one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    /// The servers this deployment runs. Empty means the capability cannot be registered.
    pub servers: Vec<aik_mcp::ServerSettings>,
}

/// What a deployment says about compacting long sessions, read from `agent.summary`.
///
/// Compaction is the one capability here that is *on* unless a deployment says otherwise,
/// and the asymmetry is deliberate. Every other switch in this file guards something that
/// reaches outside the conversation — a program that runs, a file that is written, a memory
/// that is kept. This one guards nothing: the model, the principal, the transcript and the
/// tools are the same either way. What turning it off changes is only what happens when a
/// session outgrows its budget — a recap of the oldest turns, or their silent disappearance
/// — and the second is the worse default to ship.
///
/// What it does cost is a model call per compaction, on a conversation long enough to need
/// one. A deployment that would rather forget than pay sets `enabled = false`; one that would
/// rather pay less names a smaller `model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummarySettings {
    /// Whether long sessions are compacted at all. Absent means yes.
    pub enabled: Option<bool>,
    /// The model that writes recaps, defaulting to the model that answers.
    pub model: Option<String>,
    /// How many recent records to keep when no token budget bounds the window.
    pub keep_recent: Option<usize>,
}

impl SummarySettings {
    /// Whether this deployment registers a compactor at all.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// The compactor's settings, falling back to `answering` when no model was named.
    ///
    /// The fallback is the agent's own model rather than a guess at a smaller one: a model
    /// name that is not configured anywhere is a startup failure waiting for the first long
    /// conversation, and the one model this deployment is known to be able to reach is the
    /// one it is already answering with.
    pub fn resolve(&self, answering: ModelId) -> aik_summary::SummarySettings {
        let model = self
            .model
            .clone()
            .filter(|model| non_blank(model))
            .map_or(answering, ModelId::new);
        let mut settings = aik_summary::SummarySettings::new(model);
        if let Some(keep_recent) = self.keep_recent {
            settings.keep_recent_records = keep_recent;
        }
        settings
    }
}

/// Whether unattended scheduled work actually runs in this process.
///
/// A schedule and a thing that runs it are two different capabilities, and wiring them
/// together is a deployment decision rather than a detail. Both frontends keep the same
/// durable schedule — the same database, the same jobs, the same owners — but only a process
/// that is always there should be the one firing them: a terminal session that happened to be
/// open at 3am is not a scheduler, and an unattended agent turn interleaved with somebody's
/// conversation is a surprise rather than a feature.
///
/// So the schedule is always wired and the *handler* is the switch. A job scheduled by a
/// terminal run is still stored, still owned, and still fires as soon as a host process is
/// running; it does not fire in the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobExecution {
    /// Jobs are stored and never run here.
    #[default]
    Disabled,
    /// Firings run an agent turn, under the firing's own principal.
    ///
    /// See [`AgentJobHandler`](crate::jobs::AgentJobHandler) for what that principal is and
    /// what it can therefore reach.
    Agent,
}

/// Where a deployment keeps everything that could outlive one turn.
///
/// The transcript, the agent's memories, any persistent scheduled job and the audit trail
/// share one database, so this is one decision rather than four: a deployment either has
/// somewhere durable to put them or it does not, and it cannot end up remembering facts while
/// forgetting the conversation they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Storage {
    /// No database. Everything lives for the life of the process.
    ///
    /// Persistent scheduled jobs are refused rather than accepted and forgotten, which is
    /// [`aik_scheduler`]'s rule and not something a frontend relaxes.
    Ephemeral,
    /// One database at this path, opened by [`StoreComponent`](aik_store::StoreComponent).
    Persistent(PathBuf),
}

impl Storage {
    /// The database file, if there is one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Ephemeral => None,
            Self::Persistent(path) => Some(path),
        }
    }

    /// Decides where — and whether — a deployment keeps anything durable.
    ///
    /// Precedence, highest first: `ephemeral`, an explicit path, the configured
    /// [`DATABASE_PATH_KEY`], and finally [`aik_store::default_path`]. The last of those can
    /// fail, and deliberately does rather than picking somewhere: a database of transcripts
    /// dropped into the working directory is a privacy problem an operator would not notice,
    /// and one in a temporary directory is data loss they would notice too late.
    ///
    /// `ephemeral` is first rather than third because a configuration *file* is a third
    /// source that never passes through whatever flag check a frontend does. If it were not
    /// first, "keep nothing on disk" plus a configuration file naming a database would
    /// silently open that database.
    pub fn resolve<I, K, V>(
        ephemeral: bool,
        explicit: Option<PathBuf>,
        config: &Config,
        vars: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        if ephemeral {
            return Ok(Self::Ephemeral);
        }
        if let Some(path) = explicit {
            return Ok(Self::Persistent(path));
        }
        if let Some(path) = config.value(DATABASE_PATH_KEY).and_then(Value::as_str) {
            return Ok(Self::Persistent(PathBuf::from(path)));
        }
        let vars: Vec<(String, String)> = vars
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_owned(), value.as_ref().to_owned()))
            .collect();
        aik_store::default_path(vars.iter().map(|(key, value)| (key, value))).map(Self::Persistent)
    }
}

/// Writes a resolved database path back into the tree the kernel is handed.
///
/// [`StoreSettings`](aik_store::StoreSettings) would otherwise resolve its own default from
/// the *process* environment, behind the frontend's back — which would mean a deployment
/// whose environment was supplied explicitly, as every test's is, could still open the
/// operator's real database. Pinning it here makes the path the frontend reported and the
/// path the component opens the same path by construction.
///
/// A path that is not valid UTF-8 is refused rather than transcoded. Configuration is JSON,
/// so such a path cannot survive the round trip; writing a lossy rendering of it would mean
/// the component opening a *different* file from the one that was asked for and the one that
/// was reported, which is the exact failure this function exists to prevent.
///
/// An [`Storage::Ephemeral`] deployment clears the key rather than leaving it untouched: a
/// configuration file is free to name `components.store.db.path` on its own, and a stale
/// value left behind there is exactly the kind of path a store component could still find if
/// it were ever wired in for an ephemeral run by mistake. Clearing it means that mistake
/// fails loudly resolving a database instead of silently opening the wrong one.
pub fn pin_database_path(config: Config, storage: &Storage) -> Result<Config> {
    let Some(path) = storage.path() else {
        return Ok(Config::builder()
            .layer(config.as_value().clone())
            .set(DATABASE_PATH_KEY, Value::Null)
            .build());
    };
    let path = path.to_str().ok_or_else(|| {
        Error::config(
            DATABASE_PATH_KEY,
            format!(
                "the database path `{}` is not valid UTF-8, and configuration cannot carry it \
                 unchanged; use a path that is",
                path.display()
            ),
        )
    })?;
    Ok(Config::builder()
        .layer(config.as_value().clone())
        .set(DATABASE_PATH_KEY, path)
        .build())
}

/// What is read from [`AGENT_SECTION`].
///
/// Every field here is a property of the *deployment* rather than of whichever process
/// happens to be running: who the agent is, who it acts for, which directory it is confined
/// to, which model answers, and what it is told before its first turn. A terminal and a host
/// process over one database that disagreed about any of them would be two different systems
/// sharing a file — one writing memories nobody else can find, one auditing under a name the
/// reviewer never sees, one confined to a directory the other is not.
///
/// `deny_unknown_fields` so that a misspelled key fails at startup naming itself, rather than
/// being ignored — which is the failure mode this section exists to end: a setting silently
/// absent looks exactly like an assistant that decided not to act on it.
///
/// The migration off the old per-frontend keys is made loud by the same derive on each
/// frontend's own settings struct rather than by this one: `cli.agent` and `daemon.agent` are
/// unknown fields of `cli` and `daemon`, so a configuration still naming one stops the
/// frontend that reads that section instead of quietly resolving a different principal.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentSection {
    agent: Option<String>,
    user: Option<String>,
    root: Option<PathBuf>,
    model: Option<String>,
    embedding_model: Option<String>,
    provider: Option<String>,
    system_prompt: Option<String>,
    exec: ExecSettings,
    mcp: McpSettings,
    summary: SummarySettings,
}

impl AgentSection {
    /// Reads the section, or explains what is wrong with it.
    ///
    /// A scalar where the section should be is almost always `AIK_AGENT=name`: the
    /// environment layer turns that into a *string* at `agent`, replacing the whole section.
    /// serde would report it as a type error against a struct nobody outside this file has
    /// heard of, so it is caught here and answered with the variable that was meant.
    fn read(config: &Config) -> Result<Self> {
        if let Some(value) = config.value(AGENT_SECTION)
            && !value.is_null()
            && !value.is_object()
        {
            return Err(Error::config(
                AGENT_SECTION,
                format!(
                    "`{AGENT_SECTION}` is the deployment's own section, not a single value; \
                     name the setting inside it (`{AGENT_SECTION}.agent`), or use the \
                     environment variable `{ENV_PREFIX}AGENT__AGENT`"
                ),
            ));
        }
        config.get_or_default(AGENT_SECTION)
    }
}

/// Reads the instructions pinned as the first record of every session.
///
/// One reader, used by every frontend, because the alternative is what it replaced: two
/// frontends each reading a key of their own, one shipped configuration file naming one of
/// them, and a host process that assembled the same kernel and told the agent nothing.
///
/// Whitespace-only is [`None`] rather than an empty pinned record: a prompt that says nothing
/// is not a prompt, and pinning it would spend a session's first record saying so.
pub fn system_prompt(config: &Config) -> Result<Option<String>> {
    Ok(AgentSection::read(config)?
        .system_prompt
        .filter(|prompt| non_blank(prompt)))
}

/// Whether a configured string says anything at all.
fn non_blank(value: &str) -> bool {
    !value.trim().is_empty()
}

/// The environment variable prefix every frontend layers over its configuration file.
///
/// One prefix and one section name, so `AIK_AGENT__USER` means the same thing to every
/// frontend. Two copies of this constant would be two prefixes that agree until one of them
/// is changed.
pub const ENV_PREFIX: &str = "AIK_";

/// Layers a frontend's configuration sources into one tree.
///
/// Lowest first: the configuration file, then the policy document, then the environment. The
/// policy file is wrapped as [`POLICY_SECTION`] rather than merged at the root, so the file
/// holds a policy document and reads as one instead of as a configuration tree that happens
/// to contain a policy.
///
/// Shared rather than per-frontend for the same reason everything else here is: the order of
/// these layers decides which of two sources wins, and a frontend that layered them in a
/// different order would resolve a different deployment from the same files.
pub fn load_config<I, K, V>(config: Option<&Path>, policy: Option<&Path>, vars: I) -> Result<Config>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut builder = Config::builder();
    if let Some(path) = config {
        builder = builder.layer(read_json(path, "configuration")?);
    }
    if let Some(path) = policy {
        builder = builder.layer(json!({ "policy": read_json(path, "policy")? }));
    }
    Ok(builder.env_from(ENV_PREFIX, vars).build())
}

/// Reads a JSON file, naming it in whatever goes wrong.
fn read_json(path: &Path, kind: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::wrap(
            format!("reading the {kind} file `{}`", path.display()),
            error,
        )
    })?;
    serde_json::from_str(&text)
        .map_err(|error| Error::config(path.display().to_string(), error.to_string()))
}

/// How a frontend decides where durable state lives, before configuration is consulted.
///
/// Two answers rather than one because one frontend has a mode in which the question does not
/// arise: `aik --socket` is a *client*, and a running host already holds the database
/// exclusively. See [`StorageChoice::None`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageChoice {
    /// Decide as [`Storage::resolve`] does: `--ephemeral`, an explicit path, configuration,
    /// then the XDG default.
    Resolve {
        /// Whether this process was told to keep nothing on disk.
        ephemeral: bool,
        /// A path named on the command line, if any.
        explicit: Option<PathBuf>,
    },
    /// This process opens no database whatever configuration says.
    ///
    /// Not the same statement as `--ephemeral`, which is a deployment with no durable state
    /// at all. This is a process that is not the one holding it: resolving a path anyway
    /// would mean a client refusing to start on a machine with nowhere to *put* a database it
    /// was never going to open, which is exactly the machine a thin client is most likely to
    /// be.
    None,
}

impl Default for StorageChoice {
    fn default() -> Self {
        Self::Resolve {
            ephemeral: false,
            explicit: None,
        }
    }
}

/// What a frontend contributes to resolving a deployment.
///
/// The command line, and only the command line: everything else these fields can come from is
/// read from [`AGENT_SECTION`] by [`Deployment::resolve`], which is the one place any of it is
/// interpreted. A frontend that read `agent` or `user` or `root` out of its own section would
/// be the bug this type exists to make unrepresentable — a host writing memories under one
/// principal while a terminal searched as another, over one database.
///
/// What is *not* here is what genuinely differs between frontends: a socket to connect to or
/// to bind, a connection limit, a verbosity, a one-shot prompt. Those stay in the frontend
/// that has them, because nothing about them changes how the system is assembled.
#[derive(Debug, Clone, Default)]
pub struct Deployment {
    /// The agent's identity, overriding `agent.agent`.
    pub agent: Option<String>,
    /// The user's identity, overriding `agent.user`.
    pub user: Option<String>,
    /// The confinement root, overriding `agent.root`.
    pub root: Option<PathBuf>,
    /// The model every turn is sent to, overriding `agent.model`.
    pub model: Option<String>,
    /// The model memories are embedded with, overriding `agent.embedding_model`.
    pub embedding_model: Option<String>,
    /// The provider that model is asked for, overriding `agent.provider`.
    pub provider: Option<Provider>,
    /// Which filesystem tools to register.
    pub tools: ToolSet,
    /// Which memory tools to register.
    pub memory: MemorySet,
    /// Whether programs may be run, and behind what.
    pub exec: ExecSet,
    /// Whether external MCP tool servers are started.
    pub mcp: McpSet,
    /// Whether scheduled work runs in this process.
    pub jobs: JobExecution,
    /// Where durable state goes, if anywhere.
    pub storage: StorageChoice,
}

impl Deployment {
    /// Resolves the configuration tree and these overrides into one [`RuntimeSettings`].
    ///
    /// Precedence for every deployment-wide value, highest first: the command line, then
    /// [`AGENT_SECTION`] (which the environment layer has already been merged into by
    /// [`load_config`]), then the built-in default.
    ///
    /// `vars` is the environment, supplied rather than read, because the database's default
    /// location is derived from it and a test that read the process environment would resolve
    /// to the database of whoever ran it.
    pub fn resolve<I, K, V>(&self, config: Config, vars: I) -> Result<RuntimeSettings>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let vars: Vec<(String, String)> = vars
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_owned(), value.as_ref().to_owned()))
            .collect();

        let section = AgentSection::read(&config)?;
        let provider = match (self.provider, section.provider.as_deref()) {
            (Some(provider), _) => provider,
            (None, Some(name)) => Provider::parse(name)?,
            (None, None) => Provider::default(),
        };

        let storage = match &self.storage {
            StorageChoice::None => Storage::Ephemeral,
            StorageChoice::Resolve {
                ephemeral,
                explicit,
            } => Storage::resolve(
                *ephemeral,
                explicit.clone(),
                &config,
                vars.iter().map(|(key, value)| (key, value)),
            )?,
        };
        let config = pin_database_path(config, &storage)?;

        Ok(RuntimeSettings {
            agent: AgentId::new(pick(&self.agent, &section.agent, DEFAULT_AGENT)),
            user: PrincipalId::new(pick(&self.user, &section.user, DEFAULT_USER)),
            root: resolve_root(self.root.clone().or(section.root))?,
            tools: self.tools,
            memory: self.memory,
            exec: self.exec,
            exec_settings: section.exec,
            mcp: self.mcp,
            mcp_settings: section.mcp,
            summary: section.summary,
            storage,
            jobs: self.jobs,
            system_prompt: section.system_prompt.filter(|prompt| non_blank(prompt)),
            model: self
                .model
                .clone()
                .or(section.model)
                .filter(|model| non_blank(model))
                .map(ModelId::new),
            embedding_model: self
                .embedding_model
                .clone()
                .or(section.embedding_model)
                .filter(|model| non_blank(model))
                .map(ModelId::new),
            config,
            provider,
            model_component: provider.component_id(),
        })
    }
}

/// The command line, then the configuration file, then the built-in default.
fn pick(flag: &Option<String>, file: &Option<String>, fallback: &str) -> String {
    flag.clone()
        .or_else(|| file.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

/// Settles on the directory the filesystem tools will be confined to.
///
/// Absent means the working directory, which is what a person typing `aik` in a project
/// expects and the only default that is not a guess about somebody else's filesystem.
///
/// The result is canonical wherever it can be, because the confinement boundary the tools
/// enforce *is* the canonical one: [`FsReadTool`](aik_fs::FsReadTool) and its siblings
/// canonicalize the root at construction and check every resolved path against that. Storing
/// the raw form here would mean a banner, a status reply and an audit record naming a path
/// that is not the boundary — a symlinked root would be reported as the link and enforced as
/// its target — and a reader has no way to tell the two apart.
///
/// A root that does not exist is kept as it was written rather than being made an error here.
/// Canonicalization needs the path to exist, and refusing at this point would break the two
/// deployments that legitimately have no such directory: a run with no filesystem tools at
/// all, and a client of a host process, neither of which ever touches it. A deployment that
/// *does* register the tools still fails, loudly and with the same message it always did,
/// when the tool canonicalizes the root itself.
fn resolve_root(configured: Option<PathBuf>) -> Result<PathBuf> {
    let root = match configured {
        Some(root) => root,
        None => std::env::current_dir()
            .map_err(|error| Error::wrap("resolving the current directory", error))?,
    };
    Ok(std::fs::canonicalize(&root).unwrap_or(root))
}

/// Everything assembling a system needs, with every source already resolved.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    /// The agent's identity, and the principal its tool calls are attributed to.
    pub agent: AgentId,
    /// The human the agent acts for.
    pub user: PrincipalId,
    /// The directory the filesystem tools are confined to, already resolved.
    pub root: PathBuf,
    /// Which filesystem tools to register.
    pub tools: ToolSet,
    /// Which memory tools to register.
    pub memory: MemorySet,
    /// Whether programs may be run, and behind what.
    pub exec: ExecSet,
    /// Which programs may be run, and how, read from `agent.exec`.
    pub exec_settings: ExecSettings,
    /// Whether external MCP tool servers are started in this run.
    pub mcp: McpSet,
    /// The servers this deployment describes, whatever this run does with them.
    pub mcp_settings: McpSettings,
    /// Whether long sessions are compacted, and with what, read from `agent.summary`.
    pub summary: SummarySettings,
    /// Where the durable subsystems keep what they hold, if anywhere.
    pub storage: Storage,
    /// Whether scheduled jobs are executed in this process, and by what.
    pub jobs: JobExecution,
    /// Instructions pinned as the first record of each session.
    pub system_prompt: Option<String>,
    /// The model every turn is sent to, or `None` to ask the provider for one.
    ///
    /// Deployment-wide, like every other identity here: two frontends over one database that
    /// named different models would produce one transcript answered by two assistants, and
    /// the difference would show up as the agent changing its mind rather than as a
    /// configuration mistake. `None` is resolved at startup by
    /// [`first_available_model`](crate::wiring::first_available_model), which needs a running
    /// provider and so cannot happen here.
    pub model: Option<ModelId>,
    /// The model memories are embedded with, or `None` for no semantic memory.
    ///
    /// Setting it is what turns `memory.query`'s `text` argument on: the store embeds every
    /// record it stores and every search it is given, and ranks by how close the two are.
    /// It is deliberately a *separate* setting from [`RuntimeSettings::model`] rather than
    /// being derived from it — an embedding model is a different model, usually a much
    /// smaller one, and asking a chat model to embed produces either an error or a vector
    /// nobody should be searching on.
    ///
    /// Unlike [`RuntimeSettings::model`] there is no resolution at startup for `None`: an
    /// embedding model that a deployment did not choose is one whose vectors it would be
    /// stuck with, because changing it later makes every record stored under the old one
    /// unsearchable.
    pub embedding_model: Option<ModelId>,
    /// The whole configuration tree, handed to the kernel.
    pub config: Config,
    /// Which provider serves that model.
    ///
    /// Decides which provider component [`assemble`](crate::wiring::assemble) registers.
    /// [`model_component`](RuntimeSettings::model_component) names what the agent depends on,
    /// and the two agree unless a caller deliberately points the second at a stub.
    pub provider: Provider,
    /// The component expected to publish `dyn ModelProvider`.
    ///
    /// Configurable so that the wiring can be exercised against a stub provider without a
    /// running model server; the agent component declares a dependency on it by name, and
    /// the kernel refuses to start if it is absent.
    pub model_component: ComponentId,
}

impl RuntimeSettings {
    /// The defaults, rooted at `root` and keeping nothing on disk.
    ///
    /// Ephemeral rather than durable because this constructor takes no environment and no
    /// configuration, and the one thing worse than refusing to guess a database path is
    /// guessing one. A caller that wants durability sets [`RuntimeSettings::storage`] from
    /// [`Storage::resolve`], which does consult both.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            agent: AgentId::new(DEFAULT_AGENT),
            user: PrincipalId::new(DEFAULT_USER),
            root: root.into(),
            tools: ToolSet::default(),
            memory: MemorySet::default(),
            exec: ExecSet::default(),
            exec_settings: ExecSettings::default(),
            mcp: McpSet::default(),
            mcp_settings: McpSettings::default(),
            summary: SummarySettings::default(),
            storage: Storage::Ephemeral,
            jobs: JobExecution::default(),
            system_prompt: None,
            model: None,
            embedding_model: None,
            config: Config::default(),
            provider: Provider::default(),
            model_component: Provider::default().component_id(),
        }
    }

    /// The database this deployment will open, if it opens one.
    pub fn database(&self) -> Option<&Path> {
        self.storage.path()
    }

    /// The principal the agent runs as.
    ///
    /// The agent is its own actor, delegated to by the user — never the user themselves.
    /// The distinction is the whole point of running a model behind an authorization layer:
    /// a policy has to be able to say "this person may edit anything, and the thing acting
    /// for them may not", and it can only say that if the two are different principals.
    /// [`PrincipalKind::Agent`] is what carries "acting autonomously" and
    /// [`Principal::on_behalf_of`] is what carries "for whom", so a rule can match either
    /// or both.
    pub fn principal(&self) -> Principal {
        Principal::new(self.agent.as_str(), PrincipalKind::Agent).on_behalf_of(self.user.clone())
    }

    /// The principal a *person* operating this deployment reads as.
    ///
    /// Not the agent, and not the agent acting for somebody: a human reviewing what happened
    /// is delegating nothing to a model. This is the identity the audit trail is read under —
    /// see [`aik_api::audit::AuditRecord::visible_to`], which shows a reader what they did and
    /// what was done on their behalf, so reading as the user shows the whole of what that
    /// user's agents did for them.
    pub fn operator(&self) -> Principal {
        Principal::new(self.user.clone(), PrincipalKind::User)
    }

    /// The loop's bounds and prompt for this deployment.
    ///
    /// Everything here is trusted execution metadata, fixed before the first turn: which
    /// model answers, how much may be spent, when to stop.
    pub fn loop_settings(&self, model: ModelId) -> AgentLoopSettings {
        let mut settings = AgentLoopSettings::new(model);
        settings.system_prompt = self.system_prompt.clone();
        settings
    }

    /// Whether the agent is told anything before its first turn.
    ///
    /// Absent is valid — an agent with no instructions still works — and is worth a frontend
    /// saying out loud, for the same reason an absent policy is: what it produces is an
    /// assistant that never mentions the durable memory it has, which reads as a broken
    /// memory rather than as a missing sentence.
    pub fn has_system_prompt(&self) -> bool {
        self.system_prompt.is_some()
    }

    /// One line describing what happens to a session that outgrows its budget.
    ///
    /// Said out loud by both frontends for the same reason the database path is: compaction
    /// spends a model call the person did not ask for, and its absence quietly loses the
    /// beginning of long conversations. Either way, that is something to be told rather than
    /// to discover.
    pub fn summary_notice(&self) -> String {
        if !self.summary.is_enabled() {
            return "off (long sessions lose their oldest turns silently)".to_owned();
        }
        match self
            .summary
            .model
            .as_deref()
            .filter(|model| non_blank(model))
        {
            Some(model) => format!("on ({model} recaps what a long session no longer shows)"),
            None => {
                "on (the agent's own model recaps what a long session no longer shows)".to_owned()
            }
        }
    }

    /// Whether a policy document was configured at all.
    ///
    /// An absent one is valid and denies everything, which is the right default and a
    /// baffling experience, so a frontend should say so out loud rather than letting every
    /// tool call fail mysteriously.
    pub fn has_policy(&self) -> bool {
        self.config
            .value("policy.rules")
            .and_then(Value::as_array)
            .is_some_and(|rules| !rules.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// A deployment that opens no database, so these tests do not depend on `HOME`.
    fn deployment() -> Deployment {
        Deployment {
            storage: StorageChoice::None,
            ..Deployment::default()
        }
    }

    #[test]
    fn compaction_is_on_by_default_and_uses_the_model_that_answers() {
        let settings = deployment()
            .resolve(Config::default(), env(&[]))
            .expect("defaults resolve");

        assert!(settings.summary.is_enabled());
        assert_eq!(
            settings.summary.resolve(ModelId::new("llama3.2")).model,
            ModelId::new("llama3.2"),
            "an unnamed summarising model is the one this deployment can already reach"
        );
    }

    #[test]
    fn a_deployment_can_compact_with_a_smaller_model_than_it_answers_with() {
        let config = Config::builder()
            .layer(json!({
                "agent": { "summary": { "model": "llama3.2:1b", "keep_recent": 3 } }
            }))
            .build();

        let settings = deployment().resolve(config, env(&[])).expect("resolves");
        let summary = settings.summary.resolve(ModelId::new("llama3.2"));

        assert!(settings.summary.is_enabled());
        assert_eq!(summary.model, ModelId::new("llama3.2:1b"));
        assert_eq!(summary.keep_recent_records, 3);
    }

    #[test]
    fn both_frontends_say_what_happens_to_a_long_session() {
        let mut settings = RuntimeSettings::new("/tmp");
        assert!(settings.summary_notice().starts_with("on ("));

        settings.summary.model = Some("llama3.2:1b".to_owned());
        assert!(settings.summary_notice().contains("llama3.2:1b"));

        settings.summary.enabled = Some(false);
        assert!(settings.summary_notice().starts_with("off ("));
    }

    #[test]
    fn compaction_can_be_turned_off_outright() {
        let config = Config::builder()
            .layer(json!({ "agent": { "summary": { "enabled": false } } }))
            .build();

        let settings = deployment().resolve(config, env(&[])).expect("resolves");
        assert!(!settings.summary.is_enabled());
    }

    #[test]
    fn a_misspelled_summary_key_fails_at_startup_naming_itself() {
        // The failure mode this rules out is the one the whole section exists to end: a
        // setting silently ignored looks exactly like a system that decided not to honour it.
        let config = Config::builder()
            .layer(json!({ "agent": { "summary": { "enabld": false } } }))
            .build();

        let error = deployment()
            .resolve(config, env(&[]))
            .expect_err("an unknown key is a mistake, not a default");
        assert_eq!(error.kind(), aik_core::ErrorKind::Config);
        assert!(format!("{error}").contains("enabld"), "{error}");
    }

    #[test]
    fn the_provider_defaults_to_the_one_that_needs_no_credential() {
        let settings = deployment()
            .resolve(Config::default(), env(&[]))
            .expect("defaults resolve");

        assert_eq!(settings.provider, Provider::Ollama);
        assert_eq!(
            settings.model_component,
            ComponentId::new(aik_ollama::DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn the_deployments_section_can_choose_a_hosted_provider() {
        let config = Config::builder()
            .layer(json!({ "agent": { "provider": "anthropic" } }))
            .build();

        let settings = deployment()
            .resolve(config, env(&[]))
            .expect("a named provider resolves");

        assert_eq!(settings.provider, Provider::Anthropic);
        // And the agent depends on the component that actually publishes it.
        assert_eq!(
            settings.model_component,
            ComponentId::new(aik_anthropic::DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn the_environment_layer_reaches_the_provider_like_any_other_setting() {
        let config = load_config(None, None, env(&[("AIK_AGENT__PROVIDER", "anthropic")]))
            .expect("a valid layer");

        let settings = deployment().resolve(config, env(&[])).expect("resolves");

        assert_eq!(settings.provider, Provider::Anthropic);
    }

    #[test]
    fn a_flag_outranks_the_configuration_file() {
        let config = Config::builder()
            .layer(json!({ "agent": { "provider": "anthropic" } }))
            .build();
        let chosen = Deployment {
            provider: Some(Provider::Ollama),
            ..deployment()
        };

        let settings = chosen.resolve(config, env(&[])).expect("resolves");

        assert_eq!(settings.provider, Provider::Ollama);
    }

    #[test]
    fn a_provider_nobody_implements_fails_at_startup() {
        let config = Config::builder()
            .layer(json!({ "agent": { "provider": "openai" } }))
            .build();

        let error = deployment()
            .resolve(config, env(&[]))
            .expect_err("an unknown provider is a mistake");

        assert!(format!("{error}").contains("ollama, anthropic"), "{error}");
    }

    #[test]
    fn the_agent_is_its_own_principal_acting_for_the_user() {
        let settings = RuntimeSettings::new("/tmp");
        let principal = settings.principal();

        assert_eq!(principal.kind, PrincipalKind::Agent);
        assert_eq!(principal.id.as_str(), DEFAULT_AGENT);
        assert_eq!(
            principal.on_behalf_of.as_ref().map(PrincipalId::as_str),
            Some(DEFAULT_USER),
        );
        assert_ne!(principal.id.as_str(), DEFAULT_USER);
    }

    #[test]
    fn the_agents_instructions_come_from_the_deployments_own_section() {
        let config = Config::builder()
            .layer(json!({ "agent": { "system_prompt": "you have a durable memory" } }))
            .build();

        assert_eq!(
            system_prompt(&config).expect("a valid section").as_deref(),
            Some("you have a durable memory"),
        );
    }

    #[test]
    fn an_absent_or_empty_prompt_is_nothing_rather_than_an_empty_pinned_record() {
        assert_eq!(system_prompt(&Config::default()).expect("valid"), None);

        let blank = Config::builder()
            .layer(json!({ "agent": { "system_prompt": "  \n " } }))
            .build();
        assert_eq!(system_prompt(&blank).expect("valid"), None);
    }

    #[test]
    fn a_misspelled_key_in_the_agents_section_fails_rather_than_being_ignored() {
        // The whole point of the section: an instruction that does not arrive is
        // indistinguishable, from the outside, from a model that chose not to act on it.
        let config = Config::builder()
            .layer(json!({ "agent": { "system_promt": "you have a durable memory" } }))
            .build();

        let error = system_prompt(&config).expect_err("an unknown key is a mistake");
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }

    #[test]
    fn a_prompt_that_is_not_a_string_is_refused() {
        let config = Config::builder()
            .layer(json!({ "agent": { "system_prompt": ["a", "b"] } }))
            .build();

        let error = system_prompt(&config).expect_err("a prompt is text");
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }

    #[test]
    fn a_deployment_reports_whether_its_agent_was_told_anything() {
        let mut settings = RuntimeSettings::new("/tmp");
        assert!(!settings.has_system_prompt());
        settings.system_prompt = Some("be terse".to_owned());
        assert!(settings.has_system_prompt());
    }

    #[test]
    fn the_operator_is_the_user_delegating_to_nobody() {
        let settings = RuntimeSettings::new("/tmp");
        let operator = settings.operator();

        assert_eq!(operator.kind, PrincipalKind::User);
        assert_eq!(operator.id, settings.user);
        assert_eq!(operator.on_behalf_of, None);
    }

    #[test]
    fn ephemeral_wins_over_every_other_source_of_a_database_path() {
        let config = Config::builder()
            .layer(json!({ "components": { "store": { "db": { "path": "/from/config.redb" } } } }))
            .build();

        let storage = Storage::resolve(
            true,
            Some(PathBuf::from("/from/flag.redb")),
            &config,
            env(&[("XDG_DATA_HOME", "/nonexistent")]),
        )
        .expect("resolved");

        assert_eq!(storage, Storage::Ephemeral);
        assert_eq!(storage.path(), None);
    }

    #[test]
    fn an_explicit_path_wins_over_the_configured_one() {
        let config = Config::builder()
            .layer(json!({ "components": { "store": { "db": { "path": "/from/config.redb" } } } }))
            .build();

        let configured =
            Storage::resolve(false, None, &config, env(&[])).expect("the configured path");
        assert_eq!(configured.path(), Some(Path::new("/from/config.redb")));

        let explicit = Storage::resolve(
            false,
            Some(PathBuf::from("/from/flag.redb")),
            &config,
            env(&[]),
        )
        .expect("the explicit path");
        assert_eq!(explicit.path(), Some(Path::new("/from/flag.redb")));
    }

    #[test]
    fn with_nowhere_to_put_a_database_it_refuses_rather_than_guessing() {
        let error = Storage::resolve(
            false,
            None,
            &Config::default(),
            env(&[("PATH", "/usr/bin")]),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(!error.to_string().contains("/tmp"), "{error}");
    }

    #[test]
    fn an_ephemeral_deployment_leaves_no_path_where_a_store_could_find_one() {
        let config = Config::builder()
            .layer(json!({ "components": { "store": { "db": { "path": "/from/config.redb" } } } }))
            .build();

        let pinned = pin_database_path(config, &Storage::Ephemeral).expect("pinned");
        assert!(!pinned.contains(DATABASE_PATH_KEY));
    }

    #[test]
    fn a_database_path_that_cannot_survive_configuration_is_refused() {
        use std::os::unix::ffi::OsStrExt as _;

        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe.redb"));
        let error = pin_database_path(Config::default(), &Storage::Persistent(path)).unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    fn memory_modes_round_trip_through_their_names() {
        for mode in [
            MemorySet::Off,
            MemorySet::Recall,
            MemorySet::Remember,
            MemorySet::Full,
        ] {
            assert_eq!(MemorySet::parse(mode.as_str()).expect("parsed"), mode);
        }
        assert!(MemorySet::parse("everything").is_err());
    }
}
