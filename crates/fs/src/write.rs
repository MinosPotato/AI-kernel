//! [`FsWriteTool`]: the first mutating tool, and what confining one costs.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{create_within, remaining_budget, resolve_parent_within};
use crate::tool::DEFAULT_TIMEOUT;

/// The tool name used when none is given explicitly.
pub const DEFAULT_WRITE_NAME: &str = "filesystem.write";

/// The permission required when none is given explicitly.
///
/// Deliberately distinct from [`DEFAULT_PERMISSION`](crate::DEFAULT_PERMISSION): granting a
/// principal the ability to read a directory must never imply the ability to change it, so
/// the two capabilities are never the same [`ActionId`].
pub const DEFAULT_WRITE_PERMISSION: &str = "filesystem.write";

/// The largest content this tool will write when no other limit is configured.
///
/// Symmetric with [`DEFAULT_MAX_BYTES`](crate::DEFAULT_MAX_BYTES): one call can neither pull
/// an unbounded amount of data into memory nor push an unbounded amount onto the host's
/// disk.
pub const DEFAULT_MAX_WRITE_BYTES: u64 = 1024 * 1024;

/// The permission bits a newly created file is given when no other mode is configured.
///
/// Owner read/write only. A file an agent creates is readable by the user who runs the
/// kernel and nobody else until someone decides otherwise; widening it is an explicit
/// choice ([`FsWriteTool::with_create_mode`]), never the default. Existing files keep their
/// own permissions — this tool changes contents, never modes.
pub const DEFAULT_CREATE_MODE: u32 = 0o600;

#[derive(Debug, Deserialize)]
struct FsWriteInput {
    path: String,
    content: String,
}

/// What a confined write produced, before it is turned into a [`ToolOutcome`].
enum WriteResult {
    /// The content was written in full.
    Written {
        /// How many bytes the file now holds.
        bytes: u64,
    },
    /// The content is larger than the configured limit; nothing was written or created.
    TooLarge {
        /// The configured limit, in bytes.
        limit: u64,
    },
}

/// A [`Tool`] that writes UTF-8 text files, confined to a configured root directory.
///
/// This is the counterpart to [`FsReadTool`](crate::FsReadTool) and the first capability in
/// the kernel that changes the host. It creates a file or replaces one that already exists;
/// it does not append, delete, rename, create directories, or change permissions. Each of
/// those is a distinct capability and belongs to a distinct tool, so that policy can grant
/// them separately.
///
/// # Path resolution and confinement
///
/// The `path` argument is always interpreted as **relative to the tool's root**, never as a
/// host filesystem path in its own right. A write target need not exist yet, so it cannot
/// be canonicalised the way a read target can; confinement is established on the parent
/// instead, and then anchored to a handle:
///
/// 1. Absolute paths, empty paths, and paths containing `.`, `..`, or embedded NUL bytes
///    are rejected outright — a syntactic check that runs before any filesystem access.
/// 2. The path's **parent directory** is joined onto the root and resolved with
///    [`std::fs::canonicalize`], which follows every symlink in it, and the result must
///    still be inside the root (a component-wise containment check on the canonical form,
///    not a string prefix). A parent that resolves outside the root is refused; one that
///    resolves *inside* it by way of a symlink is allowed, and the resolved location is
///    what policy is asked about.
/// 3. That directory is then opened as a handle and re-verified against the root while
///    held, and the file is opened relative to *that handle* with `openat` and
///    `O_NOFOLLOW`. See [Time-of-check to time-of-use](#time-of-check-to-time-of-use).
///
/// As with reads, this resolution is **independent of authorization**: it runs identically
/// whether or not a [`PolicyEngine`](aik_api::permission::PolicyEngine) is configured, and a
/// policy that allows everything cannot make this tool write outside its root. Policy
/// narrows what is allowed *within* the root; it cannot widen the root itself.
///
/// # Time-of-check to time-of-use
///
/// A mutating tool cannot treat this the way a reading one does. A read that is redirected
/// leaks a file; a write that is redirected destroys one, and there is no undo. So the
/// write path does not merely narrow the race — on Unix it removes the part of it that a
/// process can remove:
///
/// * **The directory is a handle, not a name.** The parent is opened with `O_DIRECTORY` and
///   `O_NOFOLLOW` and the file is opened relative to that descriptor, so once the directory
///   is open it refers to a fixed inode: renaming, replacing or swapping any directory along
///   the path afterwards cannot redirect the write, because the write no longer goes through
///   those names.
/// * **The final component is never followed.** `O_NOFOLLOW` refuses a symlink at the
///   target outright — including one pointing *inside* the root, which would otherwise let
///   the path policy authorized and the file actually written diverge.
/// * **The target must be a regular file with exactly one link.** A directory, a device
///   node or a FIFO is refused before anything is written. So is a file with more than one
///   hard link: hard links are invisible to path canonicalisation, so a second name for the
///   same inode outside the root would let a write escape confinement without any path ever
///   leaving it. Refusing multiply-linked files is the only way to rule that out from a
///   path-based boundary, and this tool prefers refusing a legitimate hard link to writing
///   through an illegitimate one.
/// * **The declared resource is the one written.** Because the final component is never
///   followed and the parent is pinned, the [`ResourceClaim`] the policy engine allowed and
///   the object that receives the bytes are the same object.
/// * **Both handles are re-checked against the root, on Linux.** The directory and then the
///   opened file are resolved back through `/proc/self/fd` and re-confined, which catches an
///   intermediate component swapped during resolution. This is the one measure that is
///   Linux-specific: other Unixes have no equivalent, and there fall back to the same
///   canonicalise-then-confine guarantee [`FsReadTool`](crate::FsReadTool) relies on.
///
/// What remains: a directory that is moved *out* of the root between being verified and
/// being opened through is caught by the check on the resulting file, but only after the
/// empty file has been created at its new location. The write is refused and no content is
/// ever written. Closing even that would need the enforcement boundary to be outside the
/// process — a mount namespace, a container, `openat2(RESOLVE_BENEATH)` — which is the
/// tool's execution environment's job, not this contract's.
///
/// On non-Unix platforms neither `openat` nor `/proc/self/fd` exists, so the write falls
/// back to a path-based create after the same parent resolution. The syntactic and
/// containment checks still hold; the handle pinning and the symlink refusal do not.
///
/// # Durability, atomicity and cancellation
///
/// The content is written from offset zero and the file is then truncated to the length
/// written, so a shorter replacement leaves no tail of the previous contents and the file is
/// never observed empty. The bytes are flushed with `fsync` before the call returns.
///
/// The write is **not atomic**. There is no temporary file and no rename, so a crash or an
/// I/O error partway through leaves a file holding a mixture of old and new bytes.
/// Atomic replacement is a different operation with different semantics — it discards the
/// target's inode, permissions and links — and is deliberately not what this tool does.
///
/// Cancellation and deadlines bound how long the caller *waits*, not how long the host
/// takes: once the underlying write has begun it runs to completion, so a call that returns
/// [`Error::Cancelled`] or [`Error::Timeout`] may still have modified the file. Both are
/// checked before any filesystem work starts, so an already-expired context never writes
/// anything at all.
///
/// # What is deliberately out of scope
///
/// Content is UTF-8 text, because that is what a JSON tool argument can carry; binary
/// writes would need a different encoding and a different size story. Oversized content is
/// reported as a structured, model-visible failure ([`ToolOutcome::error`]) and never
/// partially written. There is no append, no delete, no `mkdir -p`, and no way to create a
/// file whose parent directory does not already exist.
#[derive(Debug, Clone)]
pub struct FsWriteTool {
    name: ToolName,
    action: ActionId,
    root: PathBuf,
    max_bytes: u64,
    create_mode: u32,
    timeout: Duration,
}

impl FsWriteTool {
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
            name: ToolName::new(DEFAULT_WRITE_NAME),
            action: ActionId::new(DEFAULT_WRITE_PERMISSION),
            root,
            max_bytes: DEFAULT_MAX_WRITE_BYTES,
            create_mode: DEFAULT_CREATE_MODE,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_WRITE_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides the maximum content size this tool will write.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Overrides the permission bits given to files this tool creates.
    ///
    /// Ignored on platforms without Unix file modes, and never applied to a file that
    /// already exists.
    #[must_use]
    pub fn with_create_mode(mut self, mode: u32) -> Self {
        self.create_mode = mode;
        self
    }

    /// Overrides the default per-call timeout.
    ///
    /// A shorter [`ExecutionContext`] deadline still wins. See
    /// [the cancellation note](FsWriteTool#durability-atomicity-and-cancellation) for what a
    /// timeout does and does not stop.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The canonical root this tool is confined to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn parse(&self, arguments: Value) -> Result<FsWriteInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }

    /// The confined, canonical path a given request would write to.
    fn target(&self, requested: &str) -> Result<PathBuf> {
        let (parent, name) = resolve_parent_within(&self.root, requested)?;
        Ok(parent.join(name))
    }
}

/// Rejects a target that is not a plain, singly-linked regular file.
///
/// Runs on the already-open handle, before a single byte is written, so a refusal here
/// leaves the target exactly as it was.
fn check_target(file: &std::fs::File) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| Error::wrap("reading the target file's metadata", error))?;
    if !metadata.is_file() {
        return Err(Error::InvalidArgument(
            "the path does not name a regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(Error::Confinement(
                "the target file has more than one hard link, so writing it could modify a \
                 file outside the tool's allowed root"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Resolves, opens, verifies and writes one file. Runs on a blocking thread; see
/// [`FsWriteTool::invoke`].
fn write_confined(
    root: &Path,
    requested: &str,
    content: &str,
    max_bytes: u64,
    create_mode: u32,
) -> Result<WriteResult> {
    // Checked before the filesystem is touched at all: an oversized request must not create
    // an empty file as a side effect of being refused.
    if content.len() as u64 > max_bytes {
        return Ok(WriteResult::TooLarge { limit: max_bytes });
    }

    let (parent, name) = resolve_parent_within(root, requested)?;
    let mut file = create_within(&parent, &name, root, create_mode)?;
    check_target(&file)?;

    file.write_all(content.as_bytes())
        .map_err(|error| Error::wrap(format!("writing `{requested}`"), error))?;
    // Truncate after writing rather than before: the file is never observed empty, and a
    // replacement shorter than the previous contents leaves no tail behind either way.
    let written = content.len() as u64;
    file.set_len(written)
        .map_err(|error| Error::wrap(format!("truncating `{requested}`"), error))?;
    file.sync_all()
        .map_err(|error| Error::wrap(format!("flushing `{requested}`"), error))?;

    Ok(WriteResult::Written { bytes: written })
}

#[async_trait]
impl Tool for FsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Writes a UTF-8 text file inside this tool's configured root \
                          directory, creating it or replacing its contents entirely. \
                          `path` is always relative to that root; absolute paths, `..`, \
                          and anything that resolves outside the root are refused, as is \
                          a path whose parent directory does not already exist. Content \
                          over the configured size limit is reported as an error rather \
                          than written."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the tool's allowed root."
                    },
                    "content": {
                        "type": "string",
                        "description": "The complete new contents of the file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "bytes_written": { "type": "integer" },
                    "error": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let target = self.target(&input.path)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            ResourceId::new(target.to_string_lossy()),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        // The resource this tool writes was declared in `planned_resources` and authorized
        // before this method ran, and the write refuses to follow anything that would take
        // it elsewhere rather than discovering a new resource mid-run — so, as for
        // `FsReadTool`, there is nothing to ask the authorizer about.
        let input = self.parse(arguments)?;
        let display = input.path.clone();

        // Checked before any work starts: a cancelled or already-expired call must not
        // modify the host, and once the write below begins it cannot be interrupted.
        if cx.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let budget = remaining_budget(cx, self.timeout);
        if budget.is_zero() {
            return Err(Error::Timeout(budget));
        }

        let root = self.root.clone();
        let max_bytes = self.max_bytes;
        let create_mode = self.create_mode;
        let requested = input.path;
        let content = input.content;
        let write = tokio::task::spawn_blocking(move || {
            write_confined(&root, &requested, &content, max_bytes, create_mode)
        });

        let outcome: Result<WriteResult> = tokio::select! {
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(budget) => Err(Error::Timeout(budget)),
            joined = write => match joined {
                Ok(result) => result,
                Err(join_error) => Err(Error::wrap(
                    "filesystem write task did not complete",
                    join_error,
                )),
            },
        };
        let outcome = outcome?;

        Ok(match outcome {
            WriteResult::Written { bytes } => {
                ToolOutcome::ok(json!({ "path": display, "bytes_written": bytes }))
            }
            WriteResult::TooLarge { limit } => ToolOutcome::error(json!({
                "path": display,
                "error": format!("content exceeds the {limit}-byte write limit")
            })),
        })
    }
}
