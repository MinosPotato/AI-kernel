//! Turning options, configuration and the environment into one description of a host.
//!
//! Not "the same resolution the terminal frontend does" — literally the same function.
//! [`aik_runtime::Deployment::resolve`] reads every deployment-wide value from a section
//! neither frontend owns, so there is nothing here that could interpret `agent`, `user`,
//! `root` or `model` differently from the way `aik` interprets them. What is added here is the
//! part only a host has: where it listens, and how many clients it will serve.

use std::path::PathBuf;

use aik_api::model::ModelId;
use aik_core::prelude::*;
use aik_ipc::Endpoint;
use aik_runtime::{Deployment, JobExecution, RuntimeSettings, StorageChoice};
use serde::Deserialize;

use crate::args::Options;

/// The configuration section a host reads its own settings from.
///
/// What is left in it is genuinely a host's: where to bind, and how many clients to serve.
/// Everything describing the deployment lives in [`aik_runtime::AGENT_SECTION`], and
/// `deny_unknown_fields` on this section's own settings is what turns a configuration still naming
/// `daemon.agent` into an error rather than a principal quietly resolved from elsewhere.
pub const SECTION: &str = "daemon";

/// The environment variable prefix layered over the configuration file.
///
/// Re-exported from [`aik_runtime`] rather than declared again: `AIK_AGENT__USER` has to reach
/// a host and a terminal alike, and two constants would be two prefixes that agree until one
/// of them changes.
pub use aik_runtime::ENV_PREFIX;

/// How many clients may be connected at once when nothing says otherwise.
///
/// Generous for the thing this actually is — one person's terminals, an editor, a shell
/// widget — and small enough to be a bound rather than a formality. It exists so that a
/// client in a restart loop cannot accumulate connections until the host runs out of file
/// descriptors, which is a failure that would take the schedule down with it.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16;

/// How many requests one connection may have outstanding at once.
///
/// A conversation is one call at a time; the rest are listings and lookups. A client that
/// exceeds this is answered with a refusal on the excess rather than disconnected, because
/// the calls it already has in flight are legitimate.
pub const MAX_CALLS_IN_FLIGHT: usize = 8;

/// What a host reads from `daemon` in the configuration tree.
///
/// A socket, deliberately, even though `cli` has one too: they are not the same setting. A
/// host's is the address it *binds*, and claiming it is what makes this process the host; a
/// client's is the address it *connects to*. One deployment can legitimately have a host
/// listening where no client is configured to look, and unifying them would make that
/// unsayable while fixing nothing — a wrong socket fails immediately and visibly, which is the
/// opposite of the silent drift this section's other keys had.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileSettings {
    socket: Option<PathBuf>,
    max_connections: Option<usize>,
}

/// Everything a host needs, with every source already resolved.
#[derive(Debug, Clone)]
pub struct DaemonSettings {
    /// How the system is assembled. Shared with the terminal frontend by construction.
    pub runtime: RuntimeSettings,
    /// Where to listen, and where the token beside it goes.
    pub endpoint: Endpoint,
    /// How many clients may be connected at once.
    pub max_connections: usize,
}

impl DaemonSettings {
    /// Resolves from the process environment.
    pub fn resolve(options: &Options) -> Result<Self> {
        Self::resolve_from(options, std::env::vars().collect::<Vec<_>>())
    }

    /// As [`DaemonSettings::resolve`], with the environment supplied explicitly.
    pub fn resolve_from<I, K, V>(options: &Options, vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
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

        let deployment = Deployment {
            agent: options.agent.clone(),
            user: options.user.clone(),
            root: options.root.clone(),
            model: options.model.clone(),
            tools: options.tools(),
            memory: options.memory(),
            exec: options.exec(),
            // The whole reason this process exists: something has to actually run the
            // schedule, and it has to be something that is always there.
            jobs: JobExecution::Agent,
            storage: StorageChoice::Resolve {
                ephemeral: options.ephemeral,
                explicit: options.database.clone(),
            },
        };

        // The identities, the root, the model, the prompt and the database all come from the
        // deployment's own section, never this frontend's: see [`aik_runtime::Deployment`].
        // A host and a terminal over the same project are the same assistant, and the one
        // configuration file this repository ships has to reach both of them.
        let runtime = deployment.resolve(config, vars.iter().map(|(key, value)| (key, value)))?;

        let endpoint = Endpoint::resolve(
            options.socket.clone().or(file.socket),
            vars.iter().map(|(key, value)| (key, value)),
        )?;

        Ok(Self {
            runtime,
            endpoint,
            max_connections: options
                .max_connections
                .or(file.max_connections)
                .unwrap_or(DEFAULT_MAX_CONNECTIONS),
        })
    }

    /// The model every turn is sent to, or `None` to ask the provider for one.
    pub fn model(&self) -> Option<&ModelId> {
        self.runtime.model.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{PrincipalId, PrincipalKind};
    use aik_runtime::{DEFAULT_AGENT, DEFAULT_USER, MemorySet, Storage, ToolSet};

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn resolve(options: &Options) -> DaemonSettings {
        DaemonSettings::resolve_from(
            options,
            env(&[
                ("XDG_DATA_HOME", "/nonexistent/data"),
                ("XDG_RUNTIME_DIR", "/nonexistent/run"),
            ]),
        )
        .expect("resolved")
    }

    #[test]
    fn a_host_runs_the_schedule_and_a_terminal_does_not() {
        assert_eq!(
            resolve(&Options::default()).runtime.jobs,
            JobExecution::Agent
        );
    }

    #[test]
    fn the_agent_acts_for_the_user_and_never_as_them() {
        let settings = resolve(&Options::default());
        let principal = settings.runtime.principal();

        assert_eq!(principal.kind, PrincipalKind::Agent);
        assert_eq!(principal.id.as_str(), DEFAULT_AGENT);
        assert_eq!(
            principal.on_behalf_of.as_ref().map(PrincipalId::as_str),
            Some(DEFAULT_USER),
        );
    }

    #[test]
    fn the_defaults_land_where_the_terminal_frontend_would_look_for_them() {
        let settings = resolve(&Options::default());
        assert_eq!(
            settings.endpoint.socket(),
            std::path::Path::new("/nonexistent/run/aik/aikd.sock"),
        );
        assert_eq!(
            settings.runtime.database(),
            Some(std::path::Path::new("/nonexistent/data/aik/aik.redb")),
        );
    }

    #[test]
    fn the_command_line_wins_over_the_configuration_file() {
        // The identities come from the deployment's section and the connection limit from
        // this frontend's, which is the whole shape of the split: what a host *is* is shared,
        // what a host *does with its socket* is its own.
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(
            &path,
            r#"{ "agent": { "agent": "from-file", "user": "someone" },
                 "daemon": { "max_connections": 3 } }"#,
        )
        .expect("written");

        let settings = resolve(&Options {
            config: Some(path),
            agent: Some("from-flag".to_owned()),
            ..Options::default()
        });

        assert_eq!(settings.runtime.agent.as_str(), "from-flag");
        assert_eq!(settings.runtime.user.as_str(), "someone");
        assert_eq!(settings.max_connections, 3);
    }

    #[test]
    fn the_model_is_the_deployments_and_the_command_line_still_wins() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(&path, r#"{ "agent": { "model": "from-file" } }"#).expect("written");

        let configured = resolve(&Options {
            config: Some(path.clone()),
            ..Options::default()
        });
        assert_eq!(configured.model().map(ModelId::as_str), Some("from-file"));

        let overridden = resolve(&Options {
            config: Some(path),
            model: Some("from-flag".to_owned()),
            ..Options::default()
        });
        assert_eq!(overridden.model().map(ModelId::as_str), Some("from-flag"));
    }

    #[test]
    fn the_root_is_the_deployments_own() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path().join("project");
        std::fs::create_dir(&root).expect("a project directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(
            &path,
            serde_json::json!({ "agent": { "root": root } }).to_string(),
        )
        .expect("written");

        let settings = resolve(&Options {
            config: Some(path),
            ..Options::default()
        });
        assert_eq!(
            settings.runtime.root,
            root.canonicalize().expect("a real directory"),
        );
    }

    #[test]
    fn an_ephemeral_host_opens_no_database_and_leaves_no_path_behind() {
        let settings = resolve(&Options {
            ephemeral: true,
            ..Options::default()
        });
        assert_eq!(settings.runtime.storage, Storage::Ephemeral);
        assert!(
            !settings
                .runtime
                .config
                .contains(aik_runtime::DATABASE_PATH_KEY),
        );
    }

    #[test]
    fn narrowing_options_reach_the_wiring() {
        let settings = resolve(&Options {
            no_tools: true,
            ..Options::default()
        });
        assert_eq!(settings.runtime.tools, ToolSet::None);
        assert_eq!(settings.runtime.memory, MemorySet::Off);
    }

    #[test]
    fn the_agents_instructions_reach_a_host_from_the_deployments_own_section() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(
            &path,
            r#"{ "agent": { "system_prompt": "you have a durable memory" } }"#,
        )
        .expect("written");

        let settings = resolve(&Options {
            config: Some(path),
            ..Options::default()
        });

        assert_eq!(
            settings.runtime.system_prompt.as_deref(),
            Some("you have a durable memory"),
        );
    }

    #[test]
    fn the_shipped_configuration_tells_this_hosts_agent_about_its_memory() {
        // The regression. `docs/CLI.md` starts a host with this exact file, and the prompt in
        // it is the only thing that tells the agent its memory is durable and is never
        // recalled for it. A host that resolved it to `None` produced an assistant that would
        // store a fact when asked and then, in the next session, explain that it could not
        // look one up — which is what this file existing at all is meant to prevent.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("cli")
            .join("aik.example.json");

        let settings = resolve(&Options {
            config: Some(path),
            ..Options::default()
        });

        assert!(
            settings.runtime.has_system_prompt(),
            "the shipped configuration must reach a host as well as a terminal",
        );
        let prompt = settings
            .runtime
            .system_prompt
            .as_deref()
            .expect("a prompt just asserted present");
        assert!(prompt.contains("memory.query"), "{prompt}");
        assert!(settings.runtime.has_policy());
    }

    #[test]
    fn a_prompt_under_this_frontends_own_section_is_refused_rather_than_ignored() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(&path, r#"{ "daemon": { "system_prompt": "be terse" } }"#).expect("written");

        let error = DaemonSettings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            env(&[("XDG_RUNTIME_DIR", "/nonexistent/run")]),
        )
        .expect_err("the prompt is not this frontend's setting");
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }

    #[test]
    fn an_unknown_key_in_the_hosts_own_section_is_rejected() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(&path, r#"{ "daemon": { "sockte": "/tmp/a.sock" } }"#).expect("written");

        let error = DaemonSettings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            env(&[("XDG_RUNTIME_DIR", "/nonexistent/run")]),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }
}
