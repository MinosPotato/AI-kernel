//! What a deployment says about one tool server.
//!
//! Everything here is written by the operator and read at startup. Nothing in it can be
//! influenced by a model, by a conversation, or by the server itself: a server cannot
//! ask for another environment variable, widen its own timeout, or raise the number of
//! tools it is allowed to offer.
//!
//! The defaults are the conservative half of every choice — no environment, no network
//! configuration, bounded everything — so a deployment that fills in only `command` and
//! `args` gets the tight version rather than the convenient one.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use aik_core::{Error, Result};
use serde::Deserialize;

/// The permission every MCP tool requires unless a deployment names another.
pub const DEFAULT_PERMISSION: &str = "mcp.invoke";

/// The prefix of the [`ResourceId`](aik_api::permission::ResourceId) each call is authorized
/// against.
///
/// A policy rule matches `mcp:<server>/<tool>`, so a deployment can allow one server's
/// read-only tools and require approval for another's, without the tool names of either
/// having to be predicted when the rule is written.
pub const RESOURCE_PREFIX: &str = "mcp:";

/// The prefix of every kernel-side tool name this crate produces.
pub const NAME_PREFIX: &str = "mcp";

/// The configuration path these settings are assumed to live at, for error messages.
///
/// Named rather than hard-coded because where in a deployment's tree the servers are
/// written down is that deployment's decision — `aik-runtime` keeps them under its own
/// `agent` section — and a startup failure that names a path the operator cannot find is a
/// failure they have to guess at. See [`ServerSettings::resolve_at`].
pub const DEFAULT_SETTINGS_PATH: &str = "mcp.servers";

/// Where a server binary is looked for when a deployment names no search path.
///
/// Fixed rather than taken from the environment, for the same reason `aik-exec` fixes its
/// own: the process `PATH` is inherited from whoever started the kernel.
pub const DEFAULT_SEARCH_PATH: &str = "/usr/bin:/bin:/usr/local/bin";

/// How long one `tools/call` may take before the call is abandoned.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the `initialize` handshake may take before the server is considered unusable.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// The largest single JSON-RPC frame accepted from a server.
///
/// A frame is one line, read into memory before it can be parsed, so an unbounded reader is
/// an out-of-memory kill of the whole kernel triggered by a program the kernel started.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The largest tool result carried back to a model.
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024;

/// The largest number of tools one server may offer.
///
/// Every tool a server offers is a schema and a description in the model's tool list, on
/// every turn, for the life of the deployment. A server offering a thousand is a token bill
/// rather than a capability, and more likely a broken or hostile server than a useful one.
pub const DEFAULT_MAX_TOOLS: usize = 128;

/// How many `tools/list` pages are followed before the listing is abandoned.
pub const DEFAULT_MAX_LIST_PAGES: usize = 16;

/// One tool server a deployment runs.
///
/// `deny_unknown_fields` so that a misspelled key fails at startup naming itself, rather
/// than being ignored — which for `env` or `permission` would be a server running with a
/// different environment or a different permission than the file says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    /// The label this server is known by, which becomes the middle of every tool name it
    /// contributes (`mcp.<label>.<tool>`).
    pub label: String,
    /// The bare program name to run. Never a path: see
    /// [`aik_exec::program`].
    pub command: String,
    /// The arguments, passed as a vector. There is no shell, so nothing here is split,
    /// expanded or interpreted.
    pub args: Vec<String>,
    /// The environment the server is started with.
    ///
    /// Nothing is inherited. A server that needs `HOME`, `PATH` or a credential gets exactly
    /// what is written here — which means a deployment cannot leak the kernel's own
    /// environment (an API key for the model provider, a token in `CI`, a socket path) into
    /// a third-party program by forgetting to think about it.
    pub env: BTreeMap<String, String>,
    /// The working directory the server is started in, defaulting to the deployment's root.
    pub cwd: Option<PathBuf>,
    /// Where the command is looked for, overriding [`DEFAULT_SEARCH_PATH`].
    pub search_path: Option<String>,
    /// The permission every tool from this server requires.
    pub permission: Option<String>,
    /// The per-call wall-clock timeout, in milliseconds.
    pub call_timeout_ms: Option<u64>,
    /// How long the handshake may take, in milliseconds.
    pub startup_timeout_ms: Option<u64>,
    /// The largest single frame accepted, in bytes.
    pub max_frame_bytes: Option<usize>,
    /// The largest tool result carried back to a model, in bytes.
    pub max_result_bytes: Option<usize>,
    /// The largest number of tools this server may offer.
    pub max_tools: Option<usize>,
    /// Which of this server's tools may be exposed at all.
    ///
    /// Empty means all of them. This is the outer limit, the same shape as the program
    /// allowlist in `aik-exec`: a tool that is not listed cannot be reached however
    /// permissive the policy is, and a server that starts offering a new tool tomorrow does
    /// not silently gain it.
    pub tools: Vec<String>,
}

/// A [`ServerSettings`] whose every field has been checked and resolved.
///
/// Separate from the deserialised form so that the checks happen once, at startup, and
/// nothing downstream has to remember to make them. Constructing one is the whole
/// validation step: after this, a label is a label and a command is a program name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    /// The validated label.
    pub label: String,
    /// The validated program name.
    pub command: String,
    /// The argument vector.
    pub args: Vec<String>,
    /// The complete environment the child is given.
    pub env: BTreeMap<String, String>,
    /// The working directory, if one was named.
    pub cwd: Option<PathBuf>,
    /// The directories the command is looked for in.
    pub search_path: Vec<PathBuf>,
    /// The permission every tool from this server requires.
    pub permission: String,
    /// The per-call wall-clock budget.
    pub call_timeout: Duration,
    /// The handshake budget.
    pub startup_timeout: Duration,
    /// The largest single frame accepted.
    pub max_frame_bytes: usize,
    /// The largest result carried back to a model.
    pub max_result_bytes: usize,
    /// The largest number of tools this server may offer.
    pub max_tools: usize,
    /// The tools that may be exposed, or empty for all of them.
    pub tools: Vec<String>,
    /// Where these settings were read from, so a later failure can name it.
    pub settings_path: String,
}

/// The characters a server label may contain.
///
/// The same narrow set a remote tool name is held to, and for the same reason: the label is
/// the middle of `mcp.<label>.<tool>`, so anything that could punctuate or misrender that
/// name is refused.
fn is_label_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

impl ServerSettings {
    /// Checks every field and fills in what the deployment left out.
    ///
    /// `root` is the deployment's confinement root, used as the working directory when the
    /// settings name none — the same directory the filesystem tools are confined to, rather
    /// than a second one, so a server and the rest of the deployment agree about where this
    /// agent works.
    ///
    /// Every refusal here is a startup failure naming the setting that caused it, because
    /// each of them is a deployment that would otherwise look configured and be either
    /// useless or unsafe.
    pub fn resolve(&self, root: &std::path::Path) -> Result<ResolvedServer> {
        self.resolve_at(root, DEFAULT_SETTINGS_PATH)
    }

    /// Checks every field, naming `settings_path` in whatever it refuses.
    pub fn resolve_at(
        &self,
        root: &std::path::Path,
        settings_path: &str,
    ) -> Result<ResolvedServer> {
        let setting = |field: &str| format!("{settings_path}[{}].{field}", self.label);

        if self.label.is_empty() {
            return Err(Error::config(
                format!("{settings_path}[].label"),
                "every tool server needs a label; it is what its tools are named after",
            ));
        }
        if !self.label.bytes().all(is_label_char) {
            return Err(Error::config(
                setting("label"),
                "only ASCII letters, digits, `_` and `-` are allowed, so that a label cannot \
                 punctuate the tool namespace it is placed in",
            ));
        }

        aik_exec::program::validate_name(&self.command).map_err(|error| {
            Error::config(
                setting("command"),
                format!("{error}; a server is named, never given as a path"),
            )
        })?;

        let search_path = aik_exec::program::parse_search_path(
            self.search_path.as_deref().unwrap_or(DEFAULT_SEARCH_PATH),
        );
        if search_path.is_empty() {
            return Err(Error::config(
                setting("search_path"),
                "no absolute directory to look for the server in; relative entries are dropped, \
                 because a program a name resolves to must not depend on where the kernel was \
                 started",
            ));
        }

        for name in self.env.keys() {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(Error::config(
                    setting("env"),
                    format!("`{name}` is not a usable environment variable name"),
                ));
            }
        }

        for value in self.args.iter().chain(self.env.values()) {
            if value.contains('\0') {
                return Err(Error::config(
                    setting("args"),
                    "an argument or environment value contains a NUL, which cannot be passed to \
                     a process",
                ));
            }
        }

        for tool in &self.tools {
            crate::protocol::validate_remote_name(tool)
                .map_err(|error| Error::config(setting("tools"), format!("{error}")))?;
        }

        let permission = self
            .permission
            .clone()
            .filter(|permission| !permission.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_PERMISSION.to_owned());

        let max_tools = positive(self.max_tools, DEFAULT_MAX_TOOLS, &setting("max_tools"))?;
        let max_frame_bytes = positive(
            self.max_frame_bytes,
            DEFAULT_MAX_FRAME_BYTES,
            &setting("max_frame_bytes"),
        )?;
        let max_result_bytes = positive(
            self.max_result_bytes,
            DEFAULT_MAX_RESULT_BYTES,
            &setting("max_result_bytes"),
        )?;

        Ok(ResolvedServer {
            label: self.label.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            cwd: Some(self.cwd.clone().unwrap_or_else(|| root.to_owned())),
            search_path,
            permission,
            call_timeout: duration(self.call_timeout_ms, DEFAULT_CALL_TIMEOUT),
            startup_timeout: duration(self.startup_timeout_ms, DEFAULT_STARTUP_TIMEOUT),
            max_frame_bytes,
            max_result_bytes,
            max_tools,
            tools: self.tools.clone(),
            settings_path: settings_path.to_owned(),
        })
    }
}

impl ResolvedServer {
    /// Names one of this server's settings, for a failure that happens after startup.
    pub fn setting(&self, field: &str) -> String {
        format!("{}[{}].{field}", self.settings_path, self.label)
    }

    /// Whether `remote` is a tool this deployment exposes at all.
    pub fn exposes(&self, remote: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|allowed| allowed == remote)
    }

    /// The kernel-side name of one of this server's tools.
    pub fn tool_name(&self, remote: &str) -> String {
        format!("{NAME_PREFIX}.{}.{remote}", self.label)
    }

    /// The resource a call to one of this server's tools is authorized against.
    pub fn resource_id(&self, remote: &str) -> String {
        format!("{RESOURCE_PREFIX}{}/{remote}", self.label)
    }
}

/// Reads an optional size, refusing a zero that would silently disable something.
fn positive(value: Option<usize>, default: usize, setting: &str) -> Result<usize> {
    match value {
        None => Ok(default),
        Some(0) => Err(Error::config(
            setting.to_owned(),
            "zero is not a limit; leave the setting out for the default, or name a real one",
        )),
        Some(value) => Ok(value),
    }
}

/// Reads an optional millisecond budget, treating zero as "not configured".
fn duration(millis: Option<u64>, default: Duration) -> Duration {
    match millis {
        Some(0) | None => default,
        Some(millis) => Duration::from_millis(millis),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn settings(label: &str, command: &str) -> ServerSettings {
        ServerSettings {
            label: label.to_owned(),
            command: command.to_owned(),
            ..ServerSettings::default()
        }
    }

    #[test]
    fn defaults_are_the_conservative_half_of_every_choice() {
        let resolved = settings("files", "server")
            .resolve(std::path::Path::new("/tmp"))
            .unwrap();
        assert!(resolved.env.is_empty(), "nothing is inherited");
        assert_eq!(resolved.permission, DEFAULT_PERMISSION);
        assert_eq!(resolved.max_tools, DEFAULT_MAX_TOOLS);
        assert_eq!(resolved.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn a_label_that_could_punctuate_the_namespace_is_refused() {
        for label in ["", "a.b", "with space", "esc\u{1b}"] {
            let error = settings(label, "server")
                .resolve(std::path::Path::new("/tmp"))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Config, "{label:?}");
        }
    }

    #[test]
    fn a_command_given_as_a_path_is_refused() {
        // The failure this rules out is a configuration naming `/tmp/attacker/server`, or a
        // relative path that resolves differently depending on where the kernel was started.
        for command in ["/usr/bin/server", "./server", "../server", "srv er"] {
            let error = settings("files", command)
                .resolve(std::path::Path::new("/tmp"))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Config, "{command:?}");
        }
    }

    #[test]
    fn a_search_path_with_no_absolute_entry_is_refused() {
        let mut raw = settings("files", "server");
        raw.search_path = Some("relative:./also-relative:".into());
        let error = raw.resolve(std::path::Path::new("/tmp")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn an_unusable_environment_name_is_refused() {
        let mut raw = settings("files", "server");
        raw.env.insert("A=B".into(), "x".into());
        assert!(raw.resolve(std::path::Path::new("/tmp")).is_err());
    }

    #[test]
    fn a_zero_limit_is_refused_rather_than_read_as_unlimited() {
        let mut raw = settings("files", "server");
        raw.max_tools = Some(0);
        let error = raw.resolve(std::path::Path::new("/tmp")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn an_empty_allowlist_exposes_everything_and_a_filled_one_narrows() {
        let mut raw = settings("files", "server");
        let all = raw.resolve(std::path::Path::new("/tmp")).unwrap();
        assert!(all.exposes("anything"));

        raw.tools = vec!["read_file".into()];
        let narrowed = raw.resolve(std::path::Path::new("/tmp")).unwrap();
        assert!(narrowed.exposes("read_file"));
        assert!(!narrowed.exposes("write_file"));
    }

    #[test]
    fn names_and_resources_are_built_from_the_label() {
        let resolved = settings("files", "server")
            .resolve(std::path::Path::new("/tmp"))
            .unwrap();
        assert_eq!(resolved.tool_name("read_file"), "mcp.files.read_file");
        assert_eq!(resolved.resource_id("read_file"), "mcp:files/read_file");
    }

    #[test]
    fn an_allowlisted_tool_name_is_held_to_the_same_shape_as_a_servers_own() {
        let mut raw = settings("files", "server");
        raw.tools = vec!["not a name".into()];
        assert!(raw.resolve(std::path::Path::new("/tmp")).is_err());
    }
}
