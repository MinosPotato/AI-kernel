//! [`ExecTool`]: running one allowlisted program, behind a sandbox, under policy.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::provenance::{Reach, Trust};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::limits::Limits;
use crate::runner::{self, Completed};
use crate::sandbox::{DEFAULT_TMPFS_BYTES, Plan, Sandbox};
use crate::{command_line, program};

/// The tool name used when none is given explicitly.
pub const DEFAULT_NAME: &str = "process.execute";

/// The permission required when none is given explicitly.
pub const DEFAULT_PERMISSION: &str = "process.execute";

/// The prefix of the resource naming the program itself.
///
/// Two namespaces rather than one, so a policy rule about *which programs may run at all*
/// cannot be matched by a rule about a particular command line, or the reverse. The same
/// device `aik-memory` uses for a record's kind.
pub const PROGRAM_RESOURCE_PREFIX: &str = "program/";

/// The prefix of the resource naming the whole command.
pub const COMMAND_RESOURCE_PREFIX: &str = "command/";

/// How long one call may take when no [`ExecutionContext`] deadline (or a later one) applies.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of each output stream is kept when no other limit is configured.
///
/// Sized for a model's context rather than a terminal's scrollback: everything a command
/// produces is read, and what is over this is discarded as it arrives.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Where programs are looked for when a deployment configures no search path.
///
/// Fixed and absolute, and never the process environment's `PATH`, which is inherited from
/// whoever started the kernel.
pub const DEFAULT_SEARCH_PATH: &str = "/usr/bin:/bin";

/// The most arguments one call may pass.
pub const DEFAULT_MAX_ARGUMENTS: usize = 64;

/// The most bytes all arguments together may occupy.
pub const DEFAULT_MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// The most input one call may write to a child's standard input.
pub const DEFAULT_MAX_STDIN_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct ExecInput {
    program: String,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    stdin: Option<String>,
}

/// A [`Tool`] that runs one program from a fixed allowlist, inside a [`Sandbox`].
///
/// # What decides whether a program runs
///
/// Four independent things, in this order, and every one of them can only ever narrow:
///
/// 1. **Registration.** A deployment that does not register this tool has no way to execute
///    anything, whatever a policy document says.
/// 2. **The allowlist.** [`ExecTool::new`] takes the set of program *names* that may run.
///    A name outside it is refused before policy is consulted, before the filesystem is
///    touched, and with no way for arguments to influence the decision — the name is a bare
///    file name, never a path, so there is nothing in it for a `..` or a symlink to traverse.
/// 3. **Policy.** Each call declares two [`ResourceClaim`]s: the program
///    (`program/git`) and the whole command (`command/git status --short`). A deployment can
///    therefore allow a program outright, require a human for particular commands, or deny a
///    command shape while allowing the program — and the resource a human is asked about is
///    the command that will actually run.
/// 4. **The sandbox.** What the program can reach once it *is* running. See [`Sandbox`].
///
/// The first three are cooperative: they bound what is asked for. Only the fourth bounds what
/// happens next, which is why [`Sandbox::Unconfined`] is documented the way it is.
///
/// # There is no shell
///
/// A call names a program and supplies an argument vector. It cannot supply a command *line*.
/// Nothing here is parsed, split on whitespace, glob-expanded, or handed to `sh -c`, so there
/// is no quoting to get wrong and no metacharacter that means anything: an argument containing
/// `; rm -rf /` is one argument containing those characters, and the program receives it as
/// such.
///
/// A deployment that allowlists a shell has undone this, and undone the allowlist with it —
/// `sh` can run anything. It is not prevented, because a program allowlist cannot know what
/// every program does, and pretending otherwise would be the more dangerous design; it is
/// simply the one entry nobody should add.
///
/// # The environment is built, not inherited
///
/// The child's environment is exactly what this tool sets: a search path, a home, a temporary
/// directory, a locale, and whatever the deployment added with [`ExecTool::with_env`].
/// Nothing of the kernel's own environment reaches it — not an endpoint, not a database path,
/// not a token some host exported before starting the process.
#[derive(Debug, Clone)]
pub struct ExecTool {
    name: ToolName,
    action: ActionId,
    workspace: PathBuf,
    programs: BTreeSet<String>,
    sandbox: Sandbox,
    search_path: Vec<PathBuf>,
    search_path_raw: String,
    writable: bool,
    network: bool,
    timeout: Duration,
    max_output_bytes: usize,
    max_arguments: usize,
    max_argument_bytes: usize,
    max_stdin_bytes: usize,
    tmpfs_bytes: u64,
    limits: Option<Limits>,
    environment: Vec<(String, String)>,
}

impl ExecTool {
    /// Creates a tool that runs `programs`, working in `workspace`, confined by `sandbox`.
    ///
    /// `workspace` is canonicalised immediately and must already exist and be a directory:
    /// a tool whose working directory is unusable should fail to build rather than fail on its
    /// first call. An empty `programs` is a configuration error for the same reason — a tool
    /// that can run nothing is not a narrower deployment, it is a mistake that would only
    /// surface as a confusing refusal much later.
    pub fn new(
        workspace: impl AsRef<Path>,
        sandbox: Sandbox,
        programs: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self> {
        let requested = workspace.as_ref();
        let workspace = std::fs::canonicalize(requested).map_err(|error| {
            Error::config(
                "aik-exec.workspace",
                format!("cannot resolve `{}`: {error}", requested.display()),
            )
        })?;
        if !workspace.is_dir() {
            return Err(Error::config(
                "aik-exec.workspace",
                format!("`{}` is not a directory", requested.display()),
            ));
        }

        let mut allowed = BTreeSet::new();
        for name in programs {
            let name = name.into();
            program::validate_name(&name)
                .map_err(|error| Error::config("aik-exec.programs", format!("{error}")))?;
            allowed.insert(name);
        }
        if allowed.is_empty() {
            return Err(Error::config(
                "aik-exec.programs",
                "no programs are allowed, so this tool could never run anything; leave it \
                 unregistered instead",
            ));
        }

        Ok(Self {
            name: ToolName::new(DEFAULT_NAME),
            action: ActionId::new(DEFAULT_PERMISSION),
            workspace,
            programs: allowed,
            sandbox,
            search_path: program::parse_search_path(DEFAULT_SEARCH_PATH),
            search_path_raw: DEFAULT_SEARCH_PATH.to_owned(),
            writable: false,
            network: false,
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_arguments: DEFAULT_MAX_ARGUMENTS,
            max_argument_bytes: DEFAULT_MAX_ARGUMENT_BYTES,
            max_stdin_bytes: DEFAULT_MAX_STDIN_BYTES,
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
            limits: None,
            environment: Vec::new(),
        })
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides where programs are looked for.
    ///
    /// Relative entries are dropped: a search path that depends on the working directory means
    /// the program a name resolves to changes with where the process was started.
    #[must_use]
    pub fn with_search_path(mut self, path: impl Into<String>) -> Self {
        let raw = path.into();
        self.search_path = program::parse_search_path(&raw);
        self.search_path_raw = raw;
        self
    }

    /// Lets the child write to the workspace.
    ///
    /// Off by default. With a sandbox, the workspace is the *only* writable path the child
    /// has, so this switch is the whole difference between a program that can inspect a
    /// project and one that can change it. Without a sandbox it changes nothing that is
    /// enforced — see [`Sandbox::Unconfined`].
    #[must_use]
    pub fn writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    /// Gives the child a network.
    ///
    /// Off by default, and the default is the interesting one: a sandboxed child with no
    /// network cannot send anything it reads anywhere, whatever it reads and whatever it was
    /// told to do by a model that was told it by a file.
    #[must_use]
    pub fn with_network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Overrides the per-call wall-clock timeout.
    ///
    /// A shorter [`ExecutionContext`] deadline still wins.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Overrides how much of each output stream is kept.
    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Overrides the resource limits, instead of deriving them from the call's budget.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Overrides the size of the sandbox's `/tmp`.
    #[must_use]
    pub fn with_tmpfs_bytes(mut self, tmpfs_bytes: u64) -> Self {
        self.tmpfs_bytes = tmpfs_bytes;
        self
    }

    /// Adds one variable to the child's environment.
    ///
    /// The only way anything reaches a child's environment, because nothing is inherited. A
    /// deployment that needs `GIT_AUTHOR_NAME` sets it here, where it is visible in the
    /// wiring rather than dependent on how the kernel happened to be started.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    /// The programs this tool will run.
    pub fn programs(&self) -> impl Iterator<Item = &str> {
        self.programs.iter().map(String::as_str)
    }

    /// The directory the child works in.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// How this tool confines what it runs.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    fn parse(&self, arguments: Value) -> Result<ExecInput> {
        let input: ExecInput = serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })?;
        self.validate(&input)?;
        Ok(input)
    }

    /// Checks everything about a request that does not depend on the host.
    ///
    /// Run before resolution and before policy, so a malformed request never reaches either.
    /// The caps are not security boundaries on their own — a program allowlisted here can do
    /// what it likes with one argument — they bound what one call costs, which is the property
    /// an agent loop needs in order to be affordable and interruptible.
    fn validate(&self, input: &ExecInput) -> Result<()> {
        let name = program::validate_name(&input.program)?;
        if !self.programs.contains(name) {
            return Err(Error::InvalidArgument(format!(
                "`{name}` is not one of the programs this tool runs ({})",
                self.programs().collect::<Vec<_>>().join(", ")
            )));
        }

        if input.arguments.len() > self.max_arguments {
            return Err(Error::InvalidArgument(format!(
                "{} arguments exceeds the limit of {}",
                input.arguments.len(),
                self.max_arguments
            )));
        }
        let total: usize = input.arguments.iter().map(String::len).sum();
        if total > self.max_argument_bytes {
            return Err(Error::InvalidArgument(format!(
                "arguments total {total} bytes, over the limit of {}",
                self.max_argument_bytes
            )));
        }
        // `execve` takes NUL-terminated strings, so an argument containing one cannot be
        // passed at all. Refusing it here names the problem; letting it through would surface
        // as an opaque spawn failure.
        if let Some(argument) = input.arguments.iter().find(|a| a.contains('\0')) {
            return Err(Error::InvalidArgument(format!(
                "an argument contains a NUL byte and cannot be passed: `{}`",
                argument.replace('\0', "\\0")
            )));
        }

        if let Some(stdin) = &input.stdin
            && stdin.len() > self.max_stdin_bytes
        {
            return Err(Error::InvalidArgument(format!(
                "standard input is {} bytes, over the limit of {}",
                stdin.len(),
                self.max_stdin_bytes
            )));
        }
        Ok(())
    }

    /// The environment every child gets, in the order it is set.
    fn environment(&self) -> Vec<(String, String)> {
        let home = self.sandbox.workspace_as_seen(&self.workspace);
        let mut environment = vec![
            ("PATH".to_owned(), self.search_path_raw.clone()),
            ("HOME".to_owned(), home),
            ("TMPDIR".to_owned(), "/tmp".to_owned()),
            // A fixed locale, so that a command's output does not depend on how the host
            // running the kernel happens to be configured — which would make one deployment's
            // parsed output differ from another's for no reason anybody could see.
            ("LANG".to_owned(), "C.UTF-8".to_owned()),
            ("LC_ALL".to_owned(), "C.UTF-8".to_owned()),
        ];
        environment.extend(self.environment.iter().cloned());
        environment
    }

    /// How long this call may take, and whether that came from the caller's deadline.
    ///
    /// The distinction decides how a timeout is reported. Running past *this tool's* timeout is
    /// something a model can react to — try a narrower command — so it is a model-visible
    /// failure. Running past the *caller's* deadline means the whole operation is over, and
    /// reporting that as a tool result the model should think about would be wrong.
    fn budget(&self, cx: &ExecutionContext) -> (Duration, bool) {
        match cx.deadline {
            Some(deadline) => {
                let remaining = deadline
                    .to_system_time()
                    .duration_since(SystemTime::now())
                    .unwrap_or_default();
                if remaining < self.timeout {
                    (remaining, true)
                } else {
                    (self.timeout, false)
                }
            }
            None => (self.timeout, false),
        }
    }

    /// Turns a finished child into the JSON a model sees.
    fn report(&self, program: &str, command: &str, completed: &Completed) -> ToolOutcome {
        let mut output = json!({
            "program": program,
            "command": command,
            "exit_code": completed.code,
            "signal": completed.signal,
            "timed_out": completed.timed_out,
            "sandboxed": self.sandbox.is_enforcing(),
            "stdout": completed.stdout.text,
            "stderr": completed.stderr.text,
            "stdout_truncated": completed.stdout.truncated,
            "stderr_truncated": completed.stderr.truncated,
            "stdout_lossy": completed.stdout.lossy,
            "stderr_lossy": completed.stderr.lossy,
        });

        if completed.succeeded() {
            return ToolOutcome::ok(output);
        }

        let reason = if completed.timed_out {
            format!(
                "`{program}` was killed after {} seconds without finishing",
                self.timeout.as_secs()
            )
        } else if let Some(signal) = completed.signal {
            format!("`{program}` was killed by signal {signal}")
        } else {
            format!(
                "`{program}` exited with status {}",
                completed.code.unwrap_or(-1)
            )
        };
        if let Some(object) = output.as_object_mut() {
            object.insert("error".to_owned(), Value::String(reason));
        }
        ToolOutcome::error(output)
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn spec(&self) -> ToolSpec {
        let programs = self.programs().collect::<Vec<_>>().join(", ");
        let writable = if self.writable {
            "The working directory is writable."
        } else {
            "The working directory is read-only."
        };
        let network = if self.network {
            "The command has network access."
        } else {
            "The command has no network access."
        };
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Runs one of a fixed set of programs and returns what it printed. The \
                 available programs are: {programs}. `program` is a bare name from that list, \
                 never a path; `arguments` is a list of separate arguments, not a command \
                 line — there is no shell, so quoting, globs, pipes, redirection and `&&` mean \
                 nothing here and must not be used. The command runs in the project working \
                 directory. {writable} {network} Output over the size limit is truncated, and \
                 a command that does not finish in time is killed."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": {
                        "type": "string",
                        "description": "The program to run: one bare name from the allowed list.",
                        "enum": self.programs().collect::<Vec<_>>()
                    },
                    "arguments": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Each argument as its own string, unquoted and unsplit."
                    },
                    "stdin": {
                        "type": "string",
                        "description": "Text to write to the command's standard input, if it reads any."
                    }
                },
                "required": ["program"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "command": { "type": "string" },
                    "exit_code": { "type": ["integer", "null"] },
                    "signal": { "type": ["integer", "null"] },
                    "timed_out": { "type": "boolean" },
                    "sandboxed": { "type": "boolean" },
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "stdout_truncated": { "type": "boolean" },
                    "stderr_truncated": { "type": "boolean" },
                    "stdout_lossy": { "type": "boolean" },
                    "stderr_lossy": { "type": "boolean" },
                    "error": { "type": "string" }
                },
                "required": ["program", "command", "timed_out"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
            // Whatever the program printed. A program's stdout is not this deployment's
            // words even when the deployment chose the program.
            output_trust: Trust::Untrusted,
            // It runs host code. Even an allowlisted program is a wider reach than any
            // single-purpose tool here: what it can touch is whatever it can touch.
            reach: Reach::External,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let command = command_line::render(&input.program, &input.arguments);
        Ok(vec![
            ResourceClaim::new(
                self.action.clone(),
                ResourceId::new(format!("{PROGRAM_RESOURCE_PREFIX}{}", input.program)),
            ),
            ResourceClaim::new(
                self.action.clone(),
                ResourceId::new(format!("{COMMAND_RESOURCE_PREFIX}{command}")),
            ),
        ])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        // Both resources this call acts on — the program and the command — were declared in
        // `planned_resources` and decided before this ran. Resolution below can only ever
        // produce the program that was authorized, because the allowlist is checked against
        // the same validated name and the search path is fixed configuration, so there is
        // nothing discovered mid-run to ask about.
        let input = self.parse(arguments)?;
        let command = command_line::render(&input.program, &input.arguments);

        let program = program::resolve(&input.program, &self.search_path)?;
        let (budget, from_deadline) = self.budget(cx);

        let plan = Plan {
            program,
            arguments: input.arguments,
            workspace: self.workspace.clone(),
            writable: self.writable,
            network: self.network,
            environment: self.environment(),
            tmpfs_bytes: self.tmpfs_bytes,
        };
        let limits = self.limits.unwrap_or_else(|| Limits::for_budget(budget));

        let completed = runner::run(
            &self.sandbox,
            &plan,
            &limits,
            input.stdin.as_deref(),
            self.max_output_bytes,
            budget,
            cx,
        )
        .await?;

        // A deadline that expired is the caller's problem and not something the model can do
        // anything about; this tool's own timeout is. See `budget`.
        if completed.timed_out && from_deadline {
            return Err(Error::Timeout(budget));
        }
        Ok(self.report(&input.program, &command, &completed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn tool() -> (tempfile::TempDir, ExecTool) {
        let workspace = tempfile::tempdir().unwrap();
        let tool = ExecTool::new(
            workspace.path(),
            Sandbox::Unconfined,
            ["echo", "git", "true"],
        )
        .unwrap();
        (workspace, tool)
    }

    #[test]
    fn a_program_outside_the_allowlist_is_refused_before_anything_else() {
        let (_workspace, tool) = tool();
        let error = tool
            .planned_resources(&json!({ "program": "curl" }))
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert!(format!("{error}").contains("echo, git, true"));
    }

    #[test]
    fn a_path_is_never_accepted_as_a_program() {
        let (_workspace, tool) = tool();
        for program in ["/usr/bin/echo", "../echo", "./echo", "bin/echo"] {
            let error = tool
                .planned_resources(&json!({ "program": program }))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{program}");
        }
    }

    #[test]
    fn a_call_declares_both_the_program_and_the_whole_command() {
        let (_workspace, tool) = tool();
        let claims = tool
            .planned_resources(&json!({
                "program": "git",
                "arguments": ["log", "--oneline", "-n", "5"]
            }))
            .unwrap();

        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].resource.as_str(), "program/git");
        assert_eq!(
            claims[1].resource.as_str(),
            "command/git log --oneline -n 5"
        );
    }

    #[test]
    fn an_argument_cannot_smuggle_a_second_command_into_the_resource() {
        let (_workspace, tool) = tool();
        let claims = tool
            .planned_resources(&json!({
                "program": "echo",
                "arguments": ["hello; rm -rf /"]
            }))
            .unwrap();

        assert_eq!(
            claims[1].resource.as_str(),
            "command/echo 'hello; rm -rf /'"
        );
    }

    #[test]
    fn oversized_requests_are_refused_rather_than_run() {
        let (_workspace, tool) = tool();

        let many = json!({
            "program": "echo",
            "arguments": vec!["x"; DEFAULT_MAX_ARGUMENTS + 1]
        });
        assert_eq!(
            tool.planned_resources(&many).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );

        let long = json!({
            "program": "echo",
            "arguments": ["x".repeat(DEFAULT_MAX_ARGUMENT_BYTES + 1)]
        });
        assert_eq!(
            tool.planned_resources(&long).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );

        let input = json!({
            "program": "echo",
            "stdin": "x".repeat(DEFAULT_MAX_STDIN_BYTES + 1)
        });
        assert_eq!(
            tool.planned_resources(&input).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn an_argument_with_a_nul_byte_is_named_rather_than_failing_at_spawn() {
        let (_workspace, tool) = tool();
        let error = tool
            .planned_resources(&json!({ "program": "echo", "arguments": ["a\u{0}b"] }))
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert!(format!("{error}").contains("NUL"));
    }

    #[test]
    fn a_tool_that_could_run_nothing_is_a_configuration_error() {
        let workspace = tempfile::tempdir().unwrap();
        let error =
            ExecTool::new(workspace.path(), Sandbox::Unconfined, Vec::<String>::new()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn an_allowlist_entry_that_is_a_path_is_a_configuration_error() {
        let workspace = tempfile::tempdir().unwrap();
        let error = ExecTool::new(workspace.path(), Sandbox::Unconfined, ["/bin/sh"]).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn nothing_of_the_process_environment_is_offered_to_a_child() {
        let (_workspace, tool) = tool();
        let environment = tool.environment();
        let names: Vec<_> = environment.iter().map(|(key, _)| key.as_str()).collect();

        assert_eq!(names, ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"]);
        assert_eq!(environment[0].1, DEFAULT_SEARCH_PATH);
    }

    #[test]
    fn the_model_is_told_which_programs_exist_and_that_there_is_no_shell() {
        let (_workspace, tool) = tool();
        let spec = tool.spec();

        assert!(spec.description.contains("echo, git, true"));
        assert!(spec.description.contains("there is no shell"));
        assert!(!spec.read_only);
        assert_eq!(
            spec.required_permissions,
            vec![ActionId::new(DEFAULT_PERMISSION)]
        );
    }
}
