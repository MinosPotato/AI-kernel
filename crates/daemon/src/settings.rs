//! Turning options, configuration and the environment into one description of a host.
//!
//! The same resolution the terminal frontend does, over the same [`RuntimeSettings`], because
//! the point of that type is that both processes assemble the same deployment. What is added
//! here is the part only a host has: where it listens, and how many clients it will serve.

use std::path::PathBuf;

use aik_api::agent::AgentId;
use aik_api::model::ModelId;
use aik_api::permission::PrincipalId;
use aik_core::prelude::*;
use aik_ipc::Endpoint;
use aik_runtime::{
    DEFAULT_AGENT, DEFAULT_USER, JobExecution, RuntimeSettings, Storage, pin_database_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args::Options;

/// The configuration section a host reads its own settings from.
pub const SECTION: &str = "daemon";

/// The environment variable prefix layered over the configuration file.
pub const ENV_PREFIX: &str = "AIK_";

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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileSettings {
    model: Option<String>,
    agent: Option<String>,
    user: Option<String>,
    root: Option<PathBuf>,
    system_prompt: Option<String>,
    socket: Option<PathBuf>,
    max_connections: Option<usize>,
}

/// Everything a host needs, with every source already resolved.
#[derive(Debug, Clone)]
pub struct DaemonSettings {
    /// How the system is assembled. Shared with the terminal frontend by construction.
    pub runtime: RuntimeSettings,
    /// The model every turn is sent to, or `None` to ask the provider for one.
    pub model: Option<ModelId>,
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

        let mut builder = Config::builder();
        if let Some(path) = &options.config {
            builder = builder.layer(read_json(path, "configuration")?);
        }
        if let Some(path) = &options.policy {
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

        let storage = Storage::resolve(
            options.ephemeral,
            options.database.clone(),
            &config,
            vars.iter().map(|(key, value)| (key, value)),
        )?;
        let config = pin_database_path(config, &storage)?;

        // Built from the runtime's own defaults and then narrowed, rather than as a struct
        // literal: the model provider's component id is the wiring's to know, and naming it
        // here would be a second copy of it to keep in step.
        let mut runtime = RuntimeSettings::new(root);
        runtime.agent = AgentId::new(pick(&options.agent, &file.agent, DEFAULT_AGENT));
        runtime.user = PrincipalId::new(pick(&options.user, &file.user, DEFAULT_USER));
        runtime.tools = options.tools();
        runtime.memory = options.memory();
        runtime.storage = storage;
        // The whole reason this process exists: something has to actually run the schedule,
        // and it has to be something that is always there.
        runtime.jobs = JobExecution::Agent;
        runtime.system_prompt = file.system_prompt;
        runtime.config = config;

        let endpoint = Endpoint::resolve(
            options.socket.clone().or(file.socket),
            vars.iter().map(|(key, value)| (key, value)),
        )?;

        Ok(Self {
            runtime,
            model: options.model.clone().or(file.model).map(ModelId::new),
            endpoint,
            max_connections: options
                .max_connections
                .or(file.max_connections)
                .unwrap_or(DEFAULT_MAX_CONNECTIONS),
        })
    }
}

fn pick(flag: &Option<String>, file: &Option<String>, fallback: &str) -> String {
    flag.clone()
        .or_else(|| file.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

fn read_json(path: &std::path::Path, kind: &str) -> Result<Value> {
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
    use aik_api::permission::PrincipalKind;
    use aik_runtime::{MemorySet, ToolSet};

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
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("aikd.json");
        std::fs::write(
            &path,
            r#"{ "daemon": { "agent": "from-file", "user": "someone", "max_connections": 3 } }"#,
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
