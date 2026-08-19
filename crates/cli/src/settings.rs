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
use aik_api::agent::AgentId;
use aik_api::model::ModelId;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_core::ComponentId;
use aik_core::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args::{Options, ToolSet};

/// The configuration section this frontend reads its own settings from.
pub const SECTION: &str = "cli";

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
        let mut builder = Config::builder();
        if let Some(path) = &options.config {
            builder = builder.layer(read_json(path, "configuration")?);
        }
        if let Some(path) = &options.policy {
            // A policy file holds the document alone, so it reads as a policy rather than as
            // a configuration tree that happens to contain one.
            builder = builder.layer(json!({ "policy": read_json(path, "policy")? }));
        }
        let config = builder.env_from(ENV_PREFIX, vars).build();

        let file: FileSettings = config.get_or_default(SECTION)?;

        let root = match options.root.clone().or(file.root) {
            Some(root) => root,
            None => std::env::current_dir()
                .map_err(|error| Error::wrap("resolving the current directory", error))?,
        };

        Ok(Self {
            agent: AgentId::new(pick(&options.agent, &file.agent, DEFAULT_AGENT)),
            user: PrincipalId::new(pick(&options.user, &file.user, DEFAULT_USER)),
            model: options.model.clone().or(file.model).map(ModelId::new),
            root,
            tools: options.tools(),
            system_prompt: file.system_prompt,
            verbose: options.verbose,
            record: options.record.clone(),
            prompt: options.prompt.clone(),
            config,
            model_component: ComponentId::new(aik_ollama::DEFAULT_COMPONENT_ID),
        })
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

    fn resolve(options: &Options) -> Settings {
        Settings::resolve_from(options, Vec::<(String, String)>::new()).expect("resolved")
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

        let settings =
            Settings::resolve_from(&options, [("AIK_CLI__MODEL", "from-env")]).expect("resolved");
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
        let error = Settings::resolve_from(&options, Vec::<(String, String)>::new()).unwrap_err();
        assert!(error.to_string().contains("aik.json"), "{error}");
    }

    #[test]
    fn malformed_json_names_the_file_it_came_from() {
        let (_directory, path) = write("config.json", "{ not json");
        let options = Options {
            config: Some(path.clone()),
            ..Options::default()
        };
        let error = Settings::resolve_from(&options, Vec::<(String, String)>::new()).unwrap_err();
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
    fn an_unknown_key_in_the_frontends_own_section_is_rejected() {
        let (_directory, path) = write("config.json", r#"{ "cli": { "modle": "typo" } }"#);
        let error = Settings::resolve_from(
            &Options {
                config: Some(path),
                ..Options::default()
            },
            Vec::<(String, String)>::new(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }
}
