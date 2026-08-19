//! [`FsReadTool`] and the path resolution it enforces independently of policy.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{open_no_follow, remaining_budget, resolve_within, verify_handle_within};

/// The tool name used when none is given explicitly.
pub const DEFAULT_NAME: &str = "filesystem.read";

/// The permission required when none is given explicitly.
pub const DEFAULT_PERMISSION: &str = "filesystem.read";

/// The largest file this tool will read when no other limit is configured.
///
/// Chosen to comfortably fit source files and documents while making it impossible for one
/// call to pull an unbounded amount of data into memory or into a model's context.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// How long a single read is allowed to take when no [`ExecutionContext`] deadline (or a
/// later one) applies.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct FsReadInput {
    path: String,
}

/// What a confined read produced, before it is turned into a [`ToolOutcome`].
///
/// Distinguishing these from `Err` matters: none of them are a failure to *run* the tool —
/// they are things a model can be told about and adjust its request for, exactly the
/// distinction [`ToolOutcome`] draws between `Err` and `is_error`.
enum FileContent {
    /// The file's contents, already validated as UTF-8 text.
    Text(String),
    /// The file exists and was readable but is not valid UTF-8.
    NotUtf8,
    /// The file is larger than the configured limit; its contents were not read.
    TooLarge {
        /// The configured limit, in bytes.
        limit: u64,
    },
}

/// A read-only [`Tool`] confined to a configured root directory.
///
/// # Path resolution and confinement
///
/// The `path` argument is always interpreted as **relative to the tool's root**, never as
/// a host filesystem path in its own right:
///
/// 1. Absolute paths, empty paths, and paths containing `.`, `..`, or embedded NUL bytes
///    are rejected outright — a syntactic check that runs before any filesystem access.
/// 2. The remaining candidate is joined onto the root and resolved with
///    [`std::fs::canonicalize`], which follows every symlink in the path, including in
///    intermediate directories.
/// 3. The result must still be inside the root (a real, component-wise containment check
///    on the canonical form — not a string prefix — so `/root-secret` cannot be mistaken
///    for a path under `/root`). If not, the call is refused.
///
/// This resolution is **independent of authorization**: it runs identically whether or not
/// a [`PolicyEngine`](aik_api::permission::PolicyEngine) is configured, and a policy that
/// allows everything cannot make this tool read outside its root. Policy narrows what is
/// allowed *within* the root; it cannot widen the root itself. See
/// [`aik_api::tool#resource-level-authorization`] for how the canonical path this produces
/// becomes the [`ResourceClaim`] a policy is actually asked about.
///
/// # Time-of-check to time-of-use
///
/// The path is resolved twice: once in [`Tool::planned_resources`], to produce the
/// resource a policy is asked about, and again — independently, from scratch — at the
/// start of [`Tool::invoke`], immediately before the file is opened. Nothing computed
/// during the first resolution is reused for the second, because policy evaluation (and a
/// possible human approval) happens in between, during which the filesystem is free to
/// change.
///
/// At open time, two further measures narrow the remaining race, without closing it
/// entirely — see [`aik_api::tool#time-of-check-to-time-of-use`] for why no tool at this
/// layer can close it completely:
///
/// * The final component is opened with `O_NOFOLLOW` on Unix, so if it was replaced with a
///   symlink after resolution, the open fails instead of silently following it elsewhere.
/// * On Linux, the already-open file descriptor's real path is re-read from
///   `/proc/self/fd` and re-checked against the root, which also catches an intermediate
///   directory component being swapped during resolution.
///
/// Neither measure — nor anything else a single process can do without help from the
/// kernel or a sandbox — closes the window between resolving a path and the `openat` that
/// follows it. That is a property of the POSIX filesystem API, not of this tool; see
/// [`FsReadTool`]'s crate documentation and this crate's design report for what stronger
/// guarantees would require.
///
/// # What is deliberately out of scope
///
/// This tool only reads UTF-8 text, and only files up to a configured size
/// ([`FsReadTool::with_max_bytes`], default [`DEFAULT_MAX_BYTES`]). Binary content and
/// oversized files are reported as a structured, model-visible failure
/// ([`ToolOutcome::error`]), not an error the caller cannot react to, and never partially
/// read. There is no filesystem *write* capability anywhere in this crate.
#[derive(Debug, Clone)]
pub struct FsReadTool {
    name: ToolName,
    action: ActionId,
    root: PathBuf,
    max_bytes: u64,
    timeout: Duration,
}

impl FsReadTool {
    /// Creates a tool confined to `root`.
    ///
    /// `root` is canonicalised immediately — resolving symlinks once, at construction, so
    /// every later containment check compares against the same real directory — and must
    /// already exist and be a directory. Both are treated as configuration errors: a tool
    /// with an unusable root should fail to build, not fail on its first call.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let requested = root.as_ref();
        let root = std::fs::canonicalize(requested).map_err(|error| {
            Error::config(
                "aik-fs.root",
                format!("cannot resolve `{}`: {error}", requested.display()),
            )
        })?;
        if !root.is_dir() {
            return Err(Error::config(
                "aik-fs.root",
                format!("`{}` is not a directory", requested.display()),
            ));
        }
        Ok(Self {
            name: ToolName::new(DEFAULT_NAME),
            action: ActionId::new(DEFAULT_PERMISSION),
            root,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: DEFAULT_TIMEOUT,
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

    /// Overrides the maximum file size this tool will read.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Overrides the default per-call timeout.
    ///
    /// A shorter [`ExecutionContext`] deadline still wins, exactly as for `aik-ollama`'s
    /// requests — see [`FsReadTool`]'s crate documentation.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The canonical root this tool is confined to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn parse(&self, arguments: Value) -> Result<FsReadInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

/// Resolves, opens, verifies and reads one file. Runs on a blocking thread; see
/// [`FsReadTool::invoke`].
fn read_confined(root: &Path, requested: &str, max_bytes: u64) -> Result<FileContent> {
    let canonical = resolve_within(root, requested)?;

    let file = open_no_follow(&canonical).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found("file", requested),
        _ => Error::wrap(format!("opening `{requested}`"), error),
    })?;
    verify_handle_within(&file, root)?;

    let metadata = file
        .metadata()
        .map_err(|error| Error::wrap(format!("reading metadata for `{requested}`"), error))?;
    if !metadata.is_file() {
        return Err(Error::InvalidArgument(format!(
            "`{requested}` is not a regular file"
        )));
    }
    if metadata.len() > max_bytes {
        return Ok(FileContent::TooLarge { limit: max_bytes });
    }

    let mut buffer = Vec::new();
    (&file)
        .take(max_bytes + 1)
        .read_to_end(&mut buffer)
        .map_err(|error| Error::wrap(format!("reading `{requested}`"), error))?;
    if buffer.len() as u64 > max_bytes {
        return Ok(FileContent::TooLarge { limit: max_bytes });
    }

    match String::from_utf8(buffer) {
        Ok(text) => Ok(FileContent::Text(text)),
        Err(_) => Ok(FileContent::NotUtf8),
    }
}

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Reads a UTF-8 text file from within this tool's configured root \
                          directory. `path` is always relative to that root; absolute \
                          paths, `..`, and anything that resolves outside the root are \
                          refused. Binary files and files over the configured size limit \
                          are reported as an error rather than read."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the tool's allowed root."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "error": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: true,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let canonical = resolve_within(&self.root, &input.path)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            ResourceId::new(canonical.to_string_lossy()),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        // Every resource this tool ever touches was already declared in
        // `planned_resources` and authorized before this method ran; there is nothing
        // discovered mid-run to ask about, since a path that would resolve somewhere else
        // is refused rather than followed. See the confinement discussion above.
        let input = self.parse(arguments)?;
        let display = input.path.clone();

        let root = self.root.clone();
        let max_bytes = self.max_bytes;
        let requested = input.path;
        let read = tokio::task::spawn_blocking(move || read_confined(&root, &requested, max_bytes));

        let budget = remaining_budget(cx, self.timeout);
        let outcome: Result<FileContent> = tokio::select! {
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(budget) => Err(Error::Timeout(budget)),
            joined = read => match joined {
                Ok(result) => result,
                Err(join_error) => Err(Error::wrap(
                    "filesystem read task did not complete",
                    join_error,
                )),
            },
        };
        let outcome = outcome?;

        Ok(match outcome {
            FileContent::Text(content) => {
                ToolOutcome::ok(json!({ "path": display, "content": content }))
            }
            FileContent::NotUtf8 => ToolOutcome::error(json!({
                "path": display,
                "error": "file is not valid UTF-8 text"
            })),
            FileContent::TooLarge { limit } => ToolOutcome::error(json!({
                "path": display,
                "error": format!("file exceeds the {limit}-byte read limit")
            })),
        })
    }
}
