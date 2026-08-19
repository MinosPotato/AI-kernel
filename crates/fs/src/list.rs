//! [`FsListTool`]: the first tool that authorizes resources it only learns about while
//! running.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::fs::{File, OpenOptions};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, ErrorKind, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{remaining_budget, resolve_dir_within, verify_handle_within};
use crate::tool::DEFAULT_TIMEOUT;

/// The tool name used when none is given explicitly.
pub const DEFAULT_LIST_NAME: &str = "filesystem.list";

/// The permission required when none is given explicitly.
///
/// Deliberately distinct from [`DEFAULT_PERMISSION`](crate::DEFAULT_PERMISSION) and
/// [`DEFAULT_WRITE_PERMISSION`](crate::DEFAULT_WRITE_PERMISSION): being able to see that a
/// file exists inside a directory must never imply being able to read or write it. A
/// principal can be granted exactly one of the three and get exactly that capability.
pub const DEFAULT_LIST_PERMISSION: &str = "filesystem.list";

/// The largest number of directory entries this tool will report when no other limit is
/// configured.
///
/// Symmetric with [`DEFAULT_MAX_BYTES`](crate::DEFAULT_MAX_BYTES): one call cannot force an
/// unbounded number of entries into memory, into a model's context, or — since every entry
/// is authorized individually — into an unbounded number of authorization questions (each
/// of which may involve a human approval prompt).
pub const DEFAULT_MAX_ENTRIES: usize = 1000;

#[derive(Debug, Deserialize)]
struct FsListInput {
    #[serde(default)]
    path: Option<String>,
}

/// What kind of thing a directory entry is, as reported by `lstat` — never by following the
/// entry itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    /// A block or character device, FIFO, socket, or anything else that is none of the
    /// above.
    Other,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

fn classify(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// One entry found while scanning, before it has been authorized.
struct RawEntry {
    name: String,
    /// The entry's own canonical path — never resolved through it, even if it is itself a
    /// symlink. See [`FsListTool`]'s documentation.
    path: PathBuf,
    kind: EntryKind,
    /// Only ever `Some` for [`EntryKind::File`]: a directory's size is not meaningful here,
    /// and a symlink's size (its target's length) is not read, so as not to reveal anything
    /// about a target this tool never resolves.
    size: Option<u64>,
}

/// What a confined scan produced, before authorization is applied to each entry.
enum ScanOutcome {
    Entries(Vec<RawEntry>),
    /// More entries than the configured limit; nothing was authorized or reported.
    TooMany {
        limit: usize,
    },
}

/// A [`Tool`] that lists the immediate entries of a directory, confined to a configured root.
///
/// This is the first tool in this crate whose resources are not fully known from its
/// arguments alone: [`Tool::planned_resources`] declares only the directory itself, and the
/// entries found inside it are authorized individually, as they are discovered, through the
/// [`ResourceAuthorizer`] handed to [`Tool::invoke`] — see
/// [`aik_api::tool#resource-level-authorization`] for why that split exists. An entry a
/// policy refuses is simply left out of the result; refusing it does not fail the call, so a
/// directory containing one restricted item still lists everything else in it. Each such
/// refusal is still recorded in the audit trail as its own
/// [`AuthorizationPhase::DiscoveredResource`](aik_api::audit::AuthorizationPhase::DiscoveredResource)
/// decision, so nothing about it is actually silent — only the entry's presence in the
/// tool's output is withheld.
///
/// # What this tool does not do
///
/// It lists one directory, one level deep. It does not recurse, does not follow symlinks to
/// report what they point to (an entry that is a symlink is reported as such, never
/// resolved), and does not read any file's contents — reading is
/// [`FsReadTool`](crate::FsReadTool)'s job, and listing a directory does not grant it: the
/// two tools require distinct permissions ([`DEFAULT_LIST_PERMISSION`] and
/// [`DEFAULT_PERMISSION`](crate::DEFAULT_PERMISSION)), so a principal authorized to see that
/// a file exists is not thereby authorized to see what is in it.
///
/// # Path resolution and confinement
///
/// `path` is relative to this tool's root, exactly as for [`FsReadTool`](crate::FsReadTool)
/// and [`FsWriteTool`](crate::FsWriteTool), with one addition: it may be omitted, or given as
/// the empty string, to mean the root itself — a directory legitimately has itself as a
/// valid target in a way a file never does. Anything else follows the same rule: absolute
/// paths, `.`, `..`, and embedded NUL bytes are rejected before any filesystem access, and
/// the remaining candidate is canonicalised and checked, component-wise, for containment in
/// the root.
///
/// # Time-of-check to time-of-use
///
/// The directory itself is resolved twice, exactly as a read tool resolves its target twice
/// — once in [`Tool::planned_resources`], once fresh in [`Tool::invoke`] — and, on Unix,
/// opened with `O_NOFOLLOW` so a symlink swapped in after resolution is refused rather than
/// followed, with the resulting handle re-verified against the root. On Linux, the
/// directory's entries are then read *through that same pinned handle* (via
/// `/proc/self/fd`), not by re-opening the resolved path a second time, so renaming or
/// replacing the directory after it was verified cannot redirect what gets listed. Other
/// platforms fall back to a second, path-based open for the read — the same residual window
/// documented for [`FsWriteTool`](crate::FsWriteTool)'s non-Linux fallback.
///
/// Each entry found is a name inside an already-pinned, already-confined directory, so no
/// further resolution — and no further TOCTOU exposure — is needed to know its own path;
/// only its *type* (file, directory, symlink, other) is inspected, via `lstat`, which never
/// follows it.
#[derive(Debug, Clone)]
pub struct FsListTool {
    name: ToolName,
    action: ActionId,
    root: PathBuf,
    max_entries: usize,
    timeout: Duration,
}

impl FsListTool {
    /// Creates a tool confined to `root`.
    ///
    /// `root` is canonicalised immediately, and must already exist and be a directory —
    /// see [`FsReadTool::new`](crate::FsReadTool::new) for why this is a construction-time
    /// error rather than a first-call one.
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
            name: ToolName::new(DEFAULT_LIST_NAME),
            action: ActionId::new(DEFAULT_LIST_PERMISSION),
            root,
            max_entries: DEFAULT_MAX_ENTRIES,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_LIST_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides the maximum number of entries this tool will report for one directory.
    #[must_use]
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// Overrides the default per-call timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The canonical root this tool is confined to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn parse(&self, arguments: Value) -> Result<FsListInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

fn open_error(requested: &str, error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found("directory", requested),
        std::io::ErrorKind::NotADirectory => {
            Error::InvalidArgument(format!("`{requested}` is not a directory"))
        }
        _ => Error::wrap(format!("opening `{requested}`"), error),
    }
}

/// Turns raw directory entries into a [`ScanOutcome`], refusing (rather than truncating) a
/// directory over the configured limit.
fn build_outcome(
    dir: PathBuf,
    raw: Vec<std::fs::DirEntry>,
    max_entries: usize,
) -> Result<ScanOutcome> {
    if raw.len() > max_entries {
        return Ok(ScanOutcome::TooMany { limit: max_entries });
    }

    let mut entries = Vec::with_capacity(raw.len());
    for entry in raw {
        let file_type = entry
            .file_type()
            .map_err(|error| Error::wrap("reading a directory entry's type", error))?;
        let kind = classify(file_type);
        let size = match kind {
            EntryKind::File => entry.metadata().ok().map(|metadata| metadata.len()),
            _ => None,
        };
        entries.push(RawEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: dir.join(entry.file_name()),
            kind,
            size,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ScanOutcome::Entries(entries))
}

#[cfg(unix)]
fn open_dir_handle(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(target_os = "linux")]
fn read_dir_entries(handle: &File, _dir: &Path) -> std::io::Result<Vec<std::fs::DirEntry>> {
    // Reading through `/proc/self/fd` resolves directly to the pinned inode rather than
    // re-walking `dir`'s components, so a directory renamed or replaced after `handle` was
    // opened and verified cannot redirect what gets read here.
    use std::os::unix::io::AsRawFd;
    let proc_path = format!("/proc/self/fd/{}", handle.as_raw_fd());
    std::fs::read_dir(proc_path)?.collect()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn read_dir_entries(_handle: &File, dir: &Path) -> std::io::Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(dir)?.collect()
}

#[cfg(unix)]
fn list_confined(root: &Path, requested: &str, max_entries: usize) -> Result<ScanOutcome> {
    let dir = resolve_dir_within(root, requested)?;
    let handle = open_dir_handle(&dir).map_err(|error| open_error(requested, error))?;
    verify_handle_within(&handle, root)?;

    let raw = read_dir_entries(&handle, &dir)
        .map_err(|error| Error::wrap(format!("reading the entries of `{requested}`"), error))?;
    build_outcome(dir, raw, max_entries)
}

#[cfg(not(unix))]
fn list_confined(root: &Path, requested: &str, max_entries: usize) -> Result<ScanOutcome> {
    let dir = resolve_dir_within(root, requested)?;
    if !dir.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "`{requested}` is not a directory"
        )));
    }
    let raw: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir)
        .map_err(|error| open_error(requested, error))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| Error::wrap(format!("reading the entries of `{requested}`"), error))?;
    build_outcome(dir, raw, max_entries)
}

/// Authorizes one discovered entry, honouring `cx`'s cancellation and remaining budget the
/// same way the scan that found it did.
///
/// This is asked fresh for every entry rather than once for the whole directory, so a
/// deadline or cancellation raised while an approval is pending for one entry stops the call
/// promptly instead of waiting out however many entries remain.
async fn authorize_entry(
    authorizer: &dyn ResourceAuthorizer,
    action: &ActionId,
    resource: &ResourceId,
    cx: &ExecutionContext,
    default_timeout: Duration,
) -> Result<()> {
    let budget = remaining_budget(cx, default_timeout);
    if budget.is_zero() {
        return Err(Error::Timeout(budget));
    }
    tokio::select! {
        biased;
        () = cx.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep(budget) => Err(Error::Timeout(budget)),
        result = authorizer.authorize(action, resource) => result,
    }
}

#[async_trait]
impl Tool for FsListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Lists the immediate entries of a directory within this tool's \
                          configured root. `path` is relative to that root and may be \
                          omitted, or given as an empty string, to list the root itself; \
                          absolute paths, `..`, and anything that resolves outside the root \
                          are refused. Each entry reports its name and kind (`file`, \
                          `directory`, `symlink`, or `other`); regular files also report \
                          their size. This tool never descends into subdirectories, never \
                          follows symlinks, and never reads file contents."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list, relative to the tool's allowed \
                                        root. Omit or use an empty string to list the root \
                                        itself."
                    }
                },
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "kind": {
                                    "type": "string",
                                    "enum": ["file", "directory", "symlink", "other"]
                                },
                                "size": { "type": ["integer", "null"] }
                            },
                            "required": ["name", "kind"],
                            "additionalProperties": false
                        }
                    },
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
        let requested = input.path.unwrap_or_default();
        let dir = resolve_dir_within(&self.root, &requested)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            ResourceId::new(dir.to_string_lossy()),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;
        let requested = input.path.unwrap_or_default();
        let display = requested.clone();

        // Checked before any work starts, exactly as the write tool does: an already
        // expired or cancelled call must not touch the host at all.
        if cx.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let budget = remaining_budget(cx, self.timeout);
        if budget.is_zero() {
            return Err(Error::Timeout(budget));
        }

        let root = self.root.clone();
        let max_entries = self.max_entries;
        let scan_requested = requested.clone();
        let scan =
            tokio::task::spawn_blocking(move || list_confined(&root, &scan_requested, max_entries));

        let outcome: Result<ScanOutcome> = tokio::select! {
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(budget) => Err(Error::Timeout(budget)),
            joined = scan => match joined {
                Ok(result) => result,
                Err(join_error) => Err(Error::wrap(
                    "filesystem list task did not complete",
                    join_error,
                )),
            },
        };

        let raw_entries = match outcome? {
            ScanOutcome::TooMany { limit } => {
                return Ok(ToolOutcome::error(json!({
                    "path": display,
                    "error": format!("directory has more than the {limit}-entry listing limit")
                })));
            }
            ScanOutcome::Entries(entries) => entries,
        };

        // The directory itself was already authorized (as the planned resource); each entry
        // inside it is a resource this tool only learned about by actually reading the
        // directory, so it is authorized here, individually, as it is discovered — see
        // `FsListTool`'s documentation. A refusal removes the entry from the result without
        // failing the call; cancellation or a timeout, by contrast, aborts the whole thing.
        let mut visible = Vec::with_capacity(raw_entries.len());
        for entry in raw_entries {
            let resource = ResourceId::new(entry.path.to_string_lossy());
            match authorize_entry(authorizer, &self.action, &resource, cx, self.timeout).await {
                Ok(()) => visible.push(json!({
                    "name": entry.name,
                    "kind": entry.kind.as_str(),
                    "size": entry.size,
                })),
                Err(error) if matches!(error.kind(), ErrorKind::Cancelled | ErrorKind::Timeout) => {
                    return Err(error);
                }
                Err(_) => continue,
            }
        }

        Ok(ToolOutcome::ok(
            json!({ "path": display, "entries": visible }),
        ))
    }
}
