//! Turning command-line options and configuration files into one resolved description of
//! a run.
//!
//! Configuration comes from the kernel's own [`Config`] mechanism rather than a
//! frontend-specific format: a file, then the environment, then whatever the command line
//! overrode. Nothing here interprets a policy — the document is layered in as configuration
//! and read by [`RuleBasedPolicyEngine`](aik_policy::RuleBasedPolicyEngine), which is the
//! only thing that decides what it means.

use std::path::{Path, PathBuf};

use aik_agent::AgentLoopSettings;
use aik_api::agent::{AgentId, SessionId};
use aik_api::model::ModelId;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_core::ComponentId;
use aik_core::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args::{MemorySet, Options, ToolSet};

/// The configuration section this frontend reads its own settings from.
pub const SECTION: &str = "cli";

/// Where the shared database's path lives in the configuration tree.
///
/// The store component's own section — `components.<id>`, with the dots in `store.db`
/// nesting — so there is exactly one key for it rather than a frontend-specific alias that
/// could disagree with the one the component actually reads.
pub const DATABASE_PATH_KEY: &str = "components.store.db.path";

/// The environment variable prefix layered over the configuration file.
pub const ENV_PREFIX: &str = "AIK_";

/// The agent identity used when nothing configures one.
pub const DEFAULT_AGENT: &str = "assistant";

/// The user identity used when nothing configures one.
///
/// A fixed name rather than the host account, so a policy document written against `user`
/// means the same thing on every machine it is copied to.
pub const DEFAULT_USER: &str = "user";

/// What the frontend reads from `cli` in the configuration tree.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileSettings {
    model: Option<String>,
    agent: Option<String>,
    user: Option<String>,
    root: Option<PathBuf>,
    system_prompt: Option<String>,
}

/// Where a run keeps everything that could outlive one turn.
///
/// The transcript, the agent's memories and any persistent scheduled job share one database,
/// so this is one decision rather than three: a run either has somewhere durable to put them
/// or it does not, and a deployment cannot end up remembering facts while forgetting the
/// conversation they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Storage {
    /// No database. Everything lives for the life of the process.
    ///
    /// Persistent scheduled jobs are refused rather than accepted and forgotten, which is
    /// [`aik_scheduler`]'s rule and not something the frontend relaxes.
    Ephemeral,
    /// One database at this path, opened by
    /// [`StoreComponent`](aik_store::StoreComponent).
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
}

/// Everything a run needs, with every source already resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The agent's identity, and the principal its tool calls are attributed to.
    pub agent: AgentId,
    /// The human the agent acts for.
    pub user: PrincipalId,
    /// The model every turn is sent to, or `None` to ask the provider for one.
    pub model: Option<ModelId>,
    /// The directory the filesystem tools are confined to, already resolved.
    pub root: PathBuf,
    /// Which filesystem tools to register.
    pub tools: ToolSet,
    /// Which memory tools to register.
    pub memory: MemorySet,
    /// Where the durable subsystems keep what they hold, if anywhere.
    pub storage: Storage,
    /// Instructions pinned as the first record of each session.
    pub system_prompt: Option<String>,
    /// Whether to print authorization and context events.
    pub verbose: bool,
    /// Where to append a JSONL measurement record of the run, if anywhere.
    pub record: Option<PathBuf>,
    /// The one-shot prompt, or `None` for an interactive session.
    ///
    /// Also decides the run's security posture: a one-shot run attaches no approval
    /// responder, so anything a policy defers to a human is refused. See
    /// [`Settings::is_one_shot`].
    pub prompt: Option<String>,
    /// The durable session to resume, or `None` to start a new one.
    ///
    /// Carried through from `--session` unchanged. Nothing here decides whether it may be
    /// resumed: the frontend hands the id to the store and reports what the store says, which
    /// is what keeps the one authorization rule in the one place that has the owner.
    pub session: Option<SessionId>,
    /// The whole configuration tree, handed to the kernel.
    pub config: Config,
    /// The component expected to publish `dyn ModelProvider`.
    ///
    /// Configurable so that the wiring can be exercised against a stub provider without a
    /// running model server; the agent component declares a dependency on it by name, and
    /// the kernel refuses to start if it is absent.
    pub model_component: ComponentId,
}

impl Settings {
    /// Resolves options, configuration files and the environment into one description.
    ///
    /// Precedence, lowest first: the configuration file, the environment, the command line.
    pub fn resolve(options: &Options) -> Result<Self> {
        Self::resolve_from(options, std::env::vars().collect::<Vec<_>>())
    }

    /// As [`Settings::resolve`], with the environment supplied explicitly.
    pub fn resolve_from<I, K, V>(options: &Options, vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        // Collected rather than streamed because the environment is consulted twice: once
        // as a configuration layer, and once for the XDG variables the database's default
        // location is derived from.
        let vars: Vec<(String, String)> = vars
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_owned(), value.as_ref().to_owned()))
            .collect();

        let mut builder = Config::builder();
        if let Some(path) = &options.config {
            builder = builder.layer(read_json(path, "configuration")?);
        }
        if let Some(path) = &options.policy {
            // A policy file holds the document alone, so it reads as a policy rather than as
            // a configuration tree that happens to contain one.
            builder = builder.layer(json!({ "policy": read_json(path, "policy")? }));
        }
        let config = builder
            .env_from(ENV_PREFIX, vars.iter().map(|(key, value)| (key, value)))
            .build();

        let file: FileSettings = config.get_or_default(SECTION)?;

        let root = match options.root.clone().or(file.root) {
            Some(root) => root,
            None => std::env::current_dir()
                .map_err(|error| Error::wrap("resolving the current directory", error))?,
        };

        let storage = resolve_storage(options, &config, &vars)?;
        let config = pin_database_path(config, &storage)?;

        Ok(Self {
            agent: AgentId::new(pick(&options.agent, &file.agent, DEFAULT_AGENT)),
            user: PrincipalId::new(pick(&options.user, &file.user, DEFAULT_USER)),
            model: options.model.clone().or(file.model).map(ModelId::new),
            root,
            tools: options.tools(),
            memory: options.memory(),
            storage,
            system_prompt: file.system_prompt,
            verbose: options.verbose,
            record: options.record.clone(),
            prompt: options.prompt.clone(),
            session: options.session,
            config,
            model_component: ComponentId::new(aik_ollama::DEFAULT_COMPONENT_ID),
        })
    }

    /// The database this run will open, if it opens one.
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

    /// The loop's bounds and prompt for this run.
    ///
    /// Everything here is trusted execution metadata, fixed before the first turn: which
    /// model answers, how much may be spent, when to stop.
    pub fn loop_settings(&self, model: ModelId) -> AgentLoopSettings {
        let mut settings = AgentLoopSettings::new(model);
        settings.system_prompt = self.system_prompt.clone();
        settings
    }

    /// Whether this run answers one prompt and exits.
    ///
    /// A one-shot run attaches no [`ApprovalGate`](aik_approval::ApprovalGate), so the
    /// broker refuses every `require_approval` immediately: there is nobody to ask, and
    /// waiting out a timeout in front of an empty terminal would only turn a refusal into a
    /// slow refusal.
    pub fn is_one_shot(&self) -> bool {
        self.prompt.is_some()
    }

    /// Whether a policy document was configured at all.
    ///
    /// An absent one is valid and denies everything, which is the right default and a
    /// baffling experience, so the frontend says so out loud rather than letting every tool
    /// call fail mysteriously.
    pub fn has_policy(&self) -> bool {
        self.config
            .value("policy.rules")
            .and_then(Value::as_array)
            .is_some_and(|rules| !rules.is_empty())
    }
}

/// Decides where — and whether — this run keeps anything durable.
///
/// Precedence, highest first: `--ephemeral`, `--db`, the configured
/// [`DATABASE_PATH_KEY`], and finally [`aik_store::default_path`]. The last of those can
/// fail, and deliberately does rather than picking somewhere: a database of transcripts
/// dropped into the working directory is a privacy problem an operator would not notice,
/// and one in a temporary directory is data loss they would notice too late.
fn resolve_storage(
    options: &Options,
    config: &Config,
    vars: &[(String, String)],
) -> Result<Storage> {
    if options.ephemeral {
        return Ok(Storage::Ephemeral);
    }
    if let Some(path) = &options.database {
        return Ok(Storage::Persistent(path.clone()));
    }
    if let Some(path) = config.value(DATABASE_PATH_KEY).and_then(Value::as_str) {
        return Ok(Storage::Persistent(PathBuf::from(path)));
    }
    aik_store::default_path(vars.iter().map(|(key, value)| (key, value))).map(Storage::Persistent)
}

/// Writes the resolved database path back into the tree the kernel is handed.
///
/// [`StoreSettings`](aik_store::StoreSettings) would otherwise resolve its own default from
/// the *process* environment, behind the frontend's back — which would mean a run whose
/// environment was supplied explicitly, as every test's is, could still open the operator's
/// real database. Pinning it here makes the path the frontend reported in its banner and the
/// path the component opens the same path by construction.
///
/// A path that is not valid UTF-8 is refused rather than transcoded. Configuration is JSON,
/// so such a path cannot survive the round trip; writing a lossy rendering of it would mean
/// the component opening a *different* file from the one that was asked for and the one the
/// banner named, which is the exact failure this function exists to prevent.
///
/// An [`Storage::Ephemeral`] run clears the key rather than leaving it untouched: a
/// configuration *file* is free to name `components.store.db.path` on its own — the same
/// document might be shared with a persistent run — and a stale value left behind there is
/// exactly the kind of path a store component could still find if it were ever wired in for
/// an ephemeral run by mistake. Clearing it here means that mistake would fail loudly
/// resolving a database instead of silently opening the wrong one.
fn pin_database_path(config: Config, storage: &Storage) -> Result<Config> {
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
                "the database path `{}` is not valid UTF-8, and configuration cannot carry it unchanged; use a path that is",
                path.display()
            ),
        )
    })?;
    Ok(Config::builder()
        .layer(config.as_value().clone())
        .set(DATABASE_PATH_KEY, path)
        .build())
}

fn pick(flag: &Option<String>, file: &Option<String>, fallback: &str) -> String {
    flag.clone()
        .or_else(|| file.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join(name);
        let mut file = std::fs::File::create(&path).expect("a file");
        file.write_all(contents.as_bytes()).expect("written");
        (directory, path)
    }

    /// An XDG data root that exists nowhere.
    ///
    /// Resolution never touches the filesystem — only [`StoreComponent`] does, and no test
    /// in this module starts a kernel — so a path that cannot exist is the safest one to
    /// resolve against: if any of this ever did open the file, the test would fail loudly
    /// rather than write to whatever the machine running it happens to have.
    const FAKE_XDG: &str = "/nonexistent/xdg-data-home";

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// Resolves against an environment naming only [`FAKE_XDG`].
    ///
    /// Never the real environment: the default database path is derived from it, and a unit
    /// test that read it would resolve to the database of whoever ran it.
    fn resolve(options: &Options) -> Settings {
        Settings::resolve_from(options, env(&[("XDG_DATA_HOME", FAKE_XDG)])).expect("resolved")
    }

    #[test]
    fn the_agent_is_its_own_principal_acting_for_the_user() {
        let settings = resolve(&Options::default());
        let principal = settings.principal();

        assert_eq!(principal.kind, PrincipalKind::Agent);
        assert_eq!(principal.id.as_str(), DEFAULT_AGENT);
        assert_eq!(
            principal.on_behalf_of.as_ref().map(PrincipalId::as_str),
            Some(DEFAULT_USER),
            "the user must be reachable by policy, but must not be who is acting",
        );
        assert_ne!(
            principal.id.as_str(),
            DEFAULT_USER,
            "the frontend must never let the agent act as the user",
        );
    }

    #[test]
    fn identities_come_from_the_command_line_before_the_file() {
        let (_directory, path) = write(
            "config.json",
            r#"{ "cli": { "agent": "from-file", "user": "someone" } }"#,
        );
        let options = Options {
            config: Some(path),
            agent: Some("from-flag".to_owned()),
            ..Options::default()
        };

        let settings = resolve(&options);
        assert_eq!(settings.agent.as_str(), "from-flag");
        assert_eq!(settings.user.as_str(), "someone");
    }

    #[test]
    fn the_environment_layers_over_the_file() {
        let (_directory, path) = write("config.json", r#"{ "cli": { "model": "from-file" } }"#);
        let options = Options {
            config: Some(path),
            ..Options::default()
        };

        let settings = Settings::resolve_from(
            &options,
            env(&[("AIK_CLI__MODEL", "from-env"), ("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .expect("resolved");
        assert_eq!(
            settings.model.as_ref().map(ModelId::as_str),
            Some("from-env")
        );
    }

    #[test]
    fn a_policy_file_is_layered_in_as_the_policy_section() {
        let (_directory, path) = write(
            "policy.json",
            r#"{ "rules": [ { "action": "filesystem.read", "effect": { "decision": "allow" } } ] }"#,
        );
        let options = Options {
            policy: Some(path),
            ..Options::default()
        };

        let settings = resolve(&options);
        assert!(settings.has_policy());
        assert_eq!(
            settings.config.value("policy.rules.0.action"),
            Some(&json!("filesystem.read")),
        );
    }

    #[test]
    fn a_policy_file_overrides_the_one_in_the_configuration() {
        let (_config_dir, config) = write(
            "config.json",
            r#"{ "policy": { "rules": [ { "action": "one", "effect": { "decision": "allow" } } ] } }"#,
        );
        let (_policy_dir, policy) = write(
            "policy.json",
            r#"{ "rules": [ { "action": "two", "effect": { "decision": "allow" } } ] }"#,
        );

        let settings = resolve(&Options {
            config: Some(config),
            policy: Some(policy),
            ..Options::default()
        });
        assert_eq!(
            settings.config.value("policy.rules.0.action"),
            Some(&json!("two")),
        );
    }

    #[test]
    fn no_configured_policy_is_reported_rather_than_invented() {
        let settings = resolve(&Options::default());
        assert!(!settings.has_policy());
        assert!(
            settings.config.value("policy").is_none(),
            "the frontend must not supply a policy of its own",
        );
    }

    #[test]
    fn an_empty_rule_list_does_not_count_as_a_policy() {
        let (_directory, path) = write("policy.json", r#"{ "rules": [] }"#);
        let settings = resolve(&Options {
            policy: Some(path),
            ..Options::default()
        });
        assert!(!settings.has_policy());
    }

    #[test]
    fn a_missing_configuration_file_is_an_error_rather_than_a_default() {
        let options = Options {
            config: Some(PathBuf::from("/nonexistent/aik.json")),
            ..Options::default()
        };
        let error =
            Settings::resolve_from(&options, env(&[("XDG_DATA_HOME", FAKE_XDG)])).unwrap_err();
        assert!(error.to_string().contains("aik.json"), "{error}");
    }

    #[test]
    fn malformed_json_names_the_file_it_came_from() {
        let (_directory, path) = write("config.json", "{ not json");
        let options = Options {
            config: Some(path.clone()),
            ..Options::default()
        };
        let error =
            Settings::resolve_from(&options, env(&[("XDG_DATA_HOME", FAKE_XDG)])).unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}",
        );
    }

    #[test]
    fn the_system_prompt_reaches_the_loop_settings() {
        let (_directory, path) = write(
            "config.json",
            r#"{ "cli": { "system_prompt": "be terse" } }"#,
        );
        let settings = resolve(&Options {
            config: Some(path),
            ..Options::default()
        });

        let loop_settings = settings.loop_settings(ModelId::new("m"));
        assert_eq!(loop_settings.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(loop_settings.model.as_str(), "m");
    }

    #[test]
    fn a_run_is_durable_by_default_and_lands_under_xdg() {
        let settings = resolve(&Options::default());
        assert_eq!(
            settings.database(),
            Some(Path::new("/nonexistent/xdg-data-home/aik/aik.redb")),
            "the default has to be the store's own XDG path, not the working directory",
        );
    }

    #[test]
    fn ephemeral_opens_no_database_at_all() {
        let settings = resolve(&Options {
            ephemeral: true,
            ..Options::default()
        });
        assert_eq!(settings.storage, Storage::Ephemeral);
        assert_eq!(settings.database(), None);
        assert!(
            !settings.config.contains(DATABASE_PATH_KEY),
            "an ephemeral run must not leave a path where a store component could find one",
        );
    }

    #[test]
    fn the_flag_wins_over_the_configured_path_which_wins_over_the_environment() {
        let (_directory, path) = write(
            "config.json",
            r#"{ "components": { "store": { "db": { "path": "/from/config.redb" } } } }"#,
        );

        let configured = resolve(&Options {
            config: Some(path.clone()),
            ..Options::default()
        });
        assert_eq!(configured.database(), Some(Path::new("/from/config.redb")));

        let overridden = resolve(&Options {
            config: Some(path),
            database: Some(PathBuf::from("/from/flag.redb")),
            ..Options::default()
        });
        assert_eq!(overridden.database(), Some(Path::new("/from/flag.redb")));
    }

    #[test]
    fn ephemeral_wins_over_a_database_path_named_in_the_configuration_file() {
        // `--ephemeral` and `--db` are rejected together at the argument parser, but a
        // configured path is a third source that reaches here by a different route, and
        // never passes through that check. If ephemeral fell to third in this function's
        // precedence instead of first, `--ephemeral --config foo.json` would silently open
        // whatever database `foo.json` names — the exact leak `--ephemeral` promises not to
        // have.
        let (_directory, path) = write(
            "config.json",
            r#"{ "components": { "store": { "db": { "path": "/from/config.redb" } } } }"#,
        );

        let settings = resolve(&Options {
            config: Some(path),
            ephemeral: true,
            ..Options::default()
        });

        assert_eq!(settings.storage, Storage::Ephemeral);
        assert_eq!(settings.database(), None);
        assert!(
            !settings.config.contains(DATABASE_PATH_KEY),
            "an ephemeral run must not leave a path where a store component could find one, \
             even when the configuration file named one",
        );
    }

    #[test]
    fn the_resolved_path_is_pinned_where_the_store_component_reads_it() {
        // Otherwise `StoreSettings` resolves its own default from the *process*
        // environment, and a run whose environment was supplied explicitly — every test —
        // would open the database of whoever happened to run it.
        let settings = resolve(&Options {
            database: Some(PathBuf::from("/srv/aik/custom.redb")),
            ..Options::default()
        });
        assert_eq!(
            settings.config.value(DATABASE_PATH_KEY),
            Some(&json!("/srv/aik/custom.redb")),
        );
    }

    #[test]
    fn with_nowhere_to_put_a_database_it_refuses_rather_than_guessing() {
        let error =
            Settings::resolve_from(&Options::default(), env(&[("PATH", "/usr/bin")])).unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(
            !error.to_string().contains("/tmp"),
            "a temporary directory would be silent data loss: {error}",
        );
    }

    #[test]
    fn a_database_path_that_cannot_survive_configuration_is_refused() {
        // Lossy transcoding here would mean the store component opening a different file
        // from the one named on the command line and printed in the banner.
        use std::os::unix::ffi::OsStrExt as _;

        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe.redb"));
        let error = Settings::resolve_from(
            &Options {
                database: Some(path),
                ..Options::default()
            },
            env(&[("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    fn memory_reaches_the_settings_from_the_command_line() {
        assert_eq!(resolve(&Options::default()).memory, MemorySet::Remember);
        assert_eq!(
            resolve(&Options {
                memory: Some(MemorySet::Off),
                ..Options::default()
            })
            .memory,
            MemorySet::Off,
        );
        assert_eq!(
            resolve(&Options {
                no_tools: true,
                ..Options::default()
            })
            .memory,
            MemorySet::Off,
            "`--no-tools` has to keep meaning no tools as tools are added",
        );
    }

    #[test]
    fn an_unknown_key_in_the_frontends_own_section_is_rejected() {
        let (_directory, path) = write("config.json", r#"{ "cli": { "modle": "typo" } }"#);
        let error = Settings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            env(&[("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }
}
