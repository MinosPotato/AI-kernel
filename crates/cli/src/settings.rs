//! Turning command-line options and configuration files into one resolved description of
//! a run.
//!
//! Configuration comes from the kernel's own [`Config`] mechanism rather than a
//! frontend-specific format: a file, then the environment, then whatever the command line
//! overrode. Nothing here interprets a policy — the document is layered in as configuration
//! and read by `aik-policy`'s `RuleBasedPolicyEngine`, which is the
//! only thing that decides what it means.
//!
//! # Two halves, deliberately separated
//!
//! What this function produces is a [`RuntimeSettings`] — how the *system* is put together —
//! wrapped in the handful of things that describe how *this run* behaves: the prompt, the
//! verbosity, where a measurement file goes, whether to talk to a host process instead of
//! assembling a kernel here.
//!
//! The split is not tidiness. [`RuntimeSettings`] is what [`aik_runtime::wiring`] consumes,
//! and both frontends hand it the same type resolved by the same *function* —
//! [`aik_runtime::Deployment::resolve`] — so a deployment assembled by `aik` and one assembled
//! by `aikd` are the same deployment. Anything a terminal happens to need cannot drift into
//! that, because it does not live there.
//!
//! What this module still decides is what a terminal alone decides: which host process to
//! talk to, whether a job handler is wired here (never), and where
//! durable state comes from when this run is a client of a host rather than a host itself.
//! Every other key is read once, in [`aik_runtime::settings`], from a section neither
//! frontend owns.

use std::path::{Path, PathBuf};

use aik_agent::AgentLoopSettings;
use aik_api::agent::SessionId;
use aik_api::model::ModelId;
use aik_api::permission::Principal;
use aik_core::prelude::*;
use aik_runtime::{Deployment, JobExecution, RuntimeSettings, StorageChoice};
use serde::Deserialize;

use crate::args::Options;

/// The configuration section this frontend reads its own settings from.
///
/// What is left in it is genuinely a terminal's: which host process to talk to, if any.
/// Everything that describes the *deployment* — the agent, the user, the root, the model, the
/// prompt — lives in [`aik_runtime::AGENT_SECTION`], and `deny_unknown_fields` on
/// `deny_unknown_fields` on this section's own settings is what turns a configuration still
/// naming `cli.agent` into an error
/// rather than a value silently resolved from somewhere else.
pub const SECTION: &str = "cli";

/// Where the shared database's path lives in the configuration tree, how a run decides
/// whether it has one at all, and the environment prefix layered over the file.
///
/// Re-exported from [`aik_runtime`]: the database key names a *component's* section, so a
/// frontend-specific alias for it could only ever disagree with the component that reads it,
/// and a second copy of the environment prefix would be a second thing to keep in step with
/// `aikd`.
pub use aik_runtime::{DATABASE_PATH_KEY, ENV_PREFIX, Storage};

/// What the frontend reads from `cli` in the configuration tree.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileSettings {
    socket: Option<PathBuf>,
}

/// Everything a run needs, with every source already resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    /// How the system is assembled: identities, tools, storage, configuration.
    ///
    /// Shared with [`aik-daemon`](../aik_daemon/index.html) by construction — this is the
    /// type [`aik_runtime::wiring`] takes, not a copy of it.
    pub runtime: RuntimeSettings,
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
    /// The host process to talk to instead of assembling a kernel, if any.
    ///
    /// Present means this run is a *client*: it opens no database, registers no tool and
    /// starts no agent, because a running host process already holds all three. See
    /// [`crate::client`].
    pub socket: Option<PathBuf>,
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

        let config = aik_runtime::load_config(
            options.config.as_deref(),
            options.policy.as_deref(),
            vars.iter().map(|(key, value)| (key, value)),
        )?;

        let file: FileSettings = config.get_or_default(SECTION)?;

        let socket = resolve_socket(options, &file, &vars);

        let deployment = Deployment {
            agent: options.agent.clone(),
            user: options.user.clone(),
            root: options.root.clone(),
            model: options.model.clone(),
            embedding_model: options.embedding_model.clone(),
            provider: options.provider,
            tools: options.tools(),
            memory: options.memory(),
            exec: options.exec(),
            // A terminal is not a host process. It keeps the same durable schedule as one —
            // the same jobs, the same owners — and deliberately runs none of it: see
            // [`JobExecution`]. A conversation interrupted by a job firing at 3am is not a
            // feature, and a schedule that only advanced while somebody happened to have a
            // terminal open would be worse than one that never fired at all.
            jobs: JobExecution::Disabled,
            // A client opens no database: the host holds it, exclusively, which is the whole
            // reason there is a host. Resolving one anyway would mean a client refusing to
            // start on a machine that has nowhere to *put* a database it was never going to
            // open — which is exactly the machine a thin client is most likely to be.
            storage: match socket {
                Some(_) => StorageChoice::None,
                None => StorageChoice::Resolve {
                    ephemeral: options.ephemeral,
                    explicit: options.database.clone(),
                },
            },
        };

        // Everything deployment-wide — the identities, the root, the model, the prompt, the
        // database — is decided there and nowhere here: see [`aik_runtime::Deployment`]. A
        // terminal and a host over one project are the same assistant, and a value one of
        // them read from a key the other does not look at is two assistants answering from
        // one database.
        let runtime = deployment.resolve(config, vars.iter().map(|(key, value)| (key, value)))?;

        Ok(Self {
            runtime,
            verbose: options.verbose,
            record: options.record.clone(),
            prompt: options.prompt.clone(),
            session: options.session,
            socket,
        })
    }

    /// The model every turn is sent to, or `None` to ask the provider for one.
    pub fn model(&self) -> Option<&ModelId> {
        self.runtime.model.as_ref()
    }

    /// The database this run will open, if it opens one.
    pub fn database(&self) -> Option<&Path> {
        self.runtime.database()
    }

    /// The principal the agent runs as. See [`RuntimeSettings::principal`].
    pub fn principal(&self) -> Principal {
        self.runtime.principal()
    }

    /// The principal a person reviewing this system reads as. See
    /// [`RuntimeSettings::operator`].
    pub fn operator(&self) -> Principal {
        self.runtime.operator()
    }

    /// The loop's bounds and prompt for this run.
    pub fn loop_settings(&self, model: ModelId) -> AgentLoopSettings {
        self.runtime.loop_settings(model)
    }

    /// Whether a policy document was configured at all.
    pub fn has_policy(&self) -> bool {
        self.runtime.has_policy()
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
}

/// Decides whether this run is a client of a host process, and of which one.
///
/// Precedence, highest first: `--socket`, the configured `cli.socket`, and `$AIK_SOCKET`.
///
/// There is deliberately no fallback to the host's own default location. An absent socket
/// means "assemble a kernel here", and a default that quietly turned every `aik` into a client
/// of whatever happened to be listening would change what the command *is* based on something
/// nobody typed. `$AIK_SOCKET` exists so that an operator who wants it habitual can say so
/// once — which is the same variable `aikd` reads, so the two cannot end up naming different
/// sockets.
fn resolve_socket(
    options: &Options,
    file: &FileSettings,
    vars: &[(String, String)],
) -> Option<PathBuf> {
    options
        .socket
        .clone()
        .or_else(|| file.socket.clone())
        .or_else(|| {
            vars.iter()
                .find(|(key, value)| key == aik_ipc::SOCKET_ENV && !value.is_empty())
                .map(|(_, value)| PathBuf::from(value))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{PrincipalId, PrincipalKind};
    use aik_runtime::{DATABASE_PATH_KEY, DEFAULT_AGENT, DEFAULT_USER, MemorySet};
    use serde_json::json;
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
            r#"{ "agent": { "agent": "from-file", "user": "someone" } }"#,
        );
        let options = Options {
            config: Some(path),
            agent: Some("from-flag".to_owned()),
            ..Options::default()
        };

        let settings = resolve(&options);
        assert_eq!(settings.runtime.agent.as_str(), "from-flag");
        assert_eq!(settings.runtime.user.as_str(), "someone");
    }

    #[test]
    fn the_environment_layers_over_the_file() {
        // Over the *deployment's* section, which is the point: `AIK_AGENT__MODEL` names one
        // model for every frontend, where `AIK_CLI__MODEL` could only ever have named one for
        // this one.
        let (_directory, path) = write("config.json", r#"{ "agent": { "model": "from-file" } }"#);
        let options = Options {
            config: Some(path),
            ..Options::default()
        };

        let settings = Settings::resolve_from(
            &options,
            env(&[
                ("AIK_AGENT__MODEL", "from-env"),
                ("XDG_DATA_HOME", FAKE_XDG),
            ]),
        )
        .expect("resolved");
        assert_eq!(settings.model().map(ModelId::as_str), Some("from-env"));
    }

    #[test]
    fn the_command_line_still_wins_over_the_configured_model() {
        let (_directory, path) = write("config.json", r#"{ "agent": { "model": "from-file" } }"#);
        let settings = resolve(&Options {
            config: Some(path),
            model: Some("from-flag".to_owned()),
            ..Options::default()
        });
        assert_eq!(settings.model().map(ModelId::as_str), Some("from-flag"));
    }

    #[test]
    fn a_scalar_where_the_deployments_section_belongs_names_the_variable_that_was_meant() {
        // `AIK_AGENT=name` reads like it should set the agent's name. It does not: the
        // environment layer puts a *string* at `agent`, replacing the section wholesale. Left
        // to serde that is a type error against a struct nobody has heard of.
        let error = Settings::resolve_from(
            &Options::default(),
            env(&[("AIK_AGENT", "aikd-agent"), ("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .expect_err("a section is not a value");
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(error.to_string().contains("AIK_AGENT__AGENT"), "{error}");
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
            settings.runtime.config.value("policy.rules.0.action"),
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
            settings.runtime.config.value("policy.rules.0.action"),
            Some(&json!("two")),
        );
    }

    #[test]
    fn no_configured_policy_is_reported_rather_than_invented() {
        let settings = resolve(&Options::default());
        assert!(!settings.has_policy());
        assert!(
            settings.runtime.config.value("policy").is_none(),
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
        // From the deployment's section, which `aikd` reads too. A prompt only this frontend
        // could see would be an assistant that knows about its memory in a terminal and not
        // in a host process, over the same database.
        let (_directory, path) = write(
            "config.json",
            r#"{ "agent": { "system_prompt": "be terse" } }"#,
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
    fn a_prompt_under_this_frontends_own_section_is_refused_rather_than_ignored() {
        // What the key used to be. A configuration still naming it has to fail loudly here:
        // silently ignoring it is how a host process ended up telling its agent nothing.
        let (_directory, path) = write(
            "config.json",
            r#"{ "cli": { "system_prompt": "be terse" } }"#,
        );
        let error = Settings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            env(&[("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .expect_err("the prompt is not this frontend's setting");
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }

    #[test]
    fn a_misspelled_key_in_the_agents_section_is_refused() {
        let (_directory, path) = write(
            "config.json",
            r#"{ "agent": { "system_promt": "be terse" } }"#,
        );
        let error = Settings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            env(&[("XDG_DATA_HOME", FAKE_XDG)]),
        )
        .expect_err("an unknown key is a mistake, not a setting");
        assert!(matches!(error, Error::Config { .. }), "{error}");
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
        assert_eq!(settings.runtime.storage, Storage::Ephemeral);
        assert_eq!(settings.database(), None);
        assert!(
            !settings.runtime.config.contains(DATABASE_PATH_KEY),
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

        assert_eq!(settings.runtime.storage, Storage::Ephemeral);
        assert_eq!(settings.database(), None);
        assert!(
            !settings.runtime.config.contains(DATABASE_PATH_KEY),
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
            settings.runtime.config.value(DATABASE_PATH_KEY),
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
        assert_eq!(
            resolve(&Options::default()).runtime.memory,
            MemorySet::Remember
        );
        assert_eq!(
            resolve(&Options {
                memory: Some(MemorySet::Off),
                ..Options::default()
            })
            .runtime
            .memory,
            MemorySet::Off,
        );
        assert_eq!(
            resolve(&Options {
                no_tools: true,
                ..Options::default()
            })
            .runtime
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
