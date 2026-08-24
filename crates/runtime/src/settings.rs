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
use serde_json::Value;

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
/// `deny_unknown_fields` so that a misspelled key fails at startup naming itself, rather than
/// being ignored — which is the failure mode this section exists to end: an instruction
/// silently absent looks exactly like an assistant that decided not to act on it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentSection {
    system_prompt: Option<String>,
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
    let section: AgentSection = config.get_or_default(AGENT_SECTION)?;
    Ok(section
        .system_prompt
        .filter(|prompt| !prompt.trim().is_empty()))
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
    /// Where the durable subsystems keep what they hold, if anywhere.
    pub storage: Storage,
    /// Whether scheduled jobs are executed in this process, and by what.
    pub jobs: JobExecution,
    /// Instructions pinned as the first record of each session.
    pub system_prompt: Option<String>,
    /// The whole configuration tree, handed to the kernel.
    pub config: Config,
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
            storage: Storage::Ephemeral,
            jobs: JobExecution::default(),
            system_prompt: None,
            config: Config::default(),
            model_component: ComponentId::new(aik_ollama::DEFAULT_COMPONENT_ID),
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
