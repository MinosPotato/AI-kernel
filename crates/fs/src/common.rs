//! Path confinement, handle verification and deadline arithmetic shared by this crate's
//! tools.
//!
//! Everything here is deliberately about *names and handles*, not about reading or
//! writing: both tools resolve a caller-supplied path the same way, and any divergence
//! between how a read is confined and how a write is confined would be a security bug
//! waiting to happen. Keeping one implementation means one thing to audit.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use aik_api::execution::ExecutionContext;
use aik_core::{Error, Result};

/// Checks the syntactic form of a caller-supplied path and returns it as a [`Path`].
///
/// This runs *before* any filesystem access, so a hostile path is rejected without the
/// host ever being consulted about it. The rules are intentionally strict rather than
/// clever: a path is a sequence of plain segments, relative to a tool's root, and nothing
/// else. `.` and `..` are refused outright instead of being normalised away, because
/// normalising `a/../b` correctly in the presence of symlinks is not a string operation.
pub(crate) fn validate_relative(requested: &str) -> Result<&Path> {
    if requested.is_empty() {
        return Err(Error::InvalidArgument("path must not be empty".into()));
    }
    if requested.contains('\0') {
        return Err(Error::InvalidArgument(
            "path must not contain a NUL byte".into(),
        ));
    }

    let candidate = Path::new(requested);
    if candidate.is_absolute() {
        return Err(Error::InvalidArgument(
            "path must be relative to the tool's allowed root, not absolute".into(),
        ));
    }
    for component in candidate.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::InvalidArgument(
                "path must contain only plain segments (no `.`, `..`, or root prefixes)".into(),
            ));
        }
    }

    Ok(candidate)
}

/// Resolves `requested` — a path relative to `root` — to its canonical form, or refuses it.
///
/// The path must already exist: [`std::fs::canonicalize`] follows every symlink in it,
/// including in intermediate directories, and the result is checked component-wise for
/// containment in `root`.
///
/// This is a tool's independent enforcement boundary, described in
/// [`FsReadTool`](crate::FsReadTool)'s documentation. It is called twice per invocation
/// (once to declare the resource, once fresh before opening it) and never trusts a
/// previously-computed result.
pub(crate) fn resolve_within(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = validate_relative(requested)?;
    let joined = root.join(candidate);
    let canonical = std::fs::canonicalize(&joined).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found("file", requested),
        _ => Error::wrap(format!("resolving `{requested}`"), error),
    })?;
    confine(canonical, root)
}

/// Resolves the *parent directory* of `requested` within `root`, returning it alongside the
/// final path segment.
///
/// This is what a tool that may create a new file needs: the target itself cannot be
/// canonicalised, because it need not exist yet, but its parent must. Resolving the parent
/// and refusing to follow the final segment (see [`create_within`]) gives the same
/// guarantee by a different route — the directory the write lands in is a real, confined,
/// symlink-free directory, and the last segment is a plain name inside it.
pub(crate) fn resolve_parent_within(root: &Path, requested: &str) -> Result<(PathBuf, OsString)> {
    let candidate = validate_relative(requested)?;
    let name = candidate
        .file_name()
        .ok_or_else(|| Error::InvalidArgument("path must name a file".into()))?
        .to_owned();
    // Every component is `Normal`, so the parent is either a plain relative path or empty,
    // which joins onto the root unchanged.
    let parent = candidate.parent().unwrap_or_else(|| Path::new(""));

    let joined = root.join(parent);
    let canonical = std::fs::canonicalize(&joined).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found("directory", parent.display().to_string()),
        _ => Error::wrap(format!("resolving the parent of `{requested}`"), error),
    })?;

    Ok((confine(canonical, root)?, name))
}

/// Refuses a resolved path that escaped `root`.
///
/// The check is component-wise on the canonical form, not a string prefix, so `/root-secret`
/// cannot be mistaken for a path under `/root`.
fn confine(canonical: PathBuf, root: &Path) -> Result<PathBuf> {
    if canonical.starts_with(root) {
        Ok(canonical)
    } else {
        Err(Error::InvalidArgument(
            "path resolves outside the tool's allowed root".into(),
        ))
    }
}

/// Opens `path` for reading, refusing to follow a symlink in its final component on Unix.
///
/// This does not, on its own, close the resolve-then-open race — see
/// [`FsReadTool`](crate::FsReadTool)'s documentation — but it does turn the single most
/// common exploitation of it (swap the target file for a symlink between resolution and
/// open) into an open failure instead of a silent redirect.
pub(crate) fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Re-checks an already-open file's or directory's real path against `root`, catching an
/// intermediate component that was swapped during resolution — something `O_NOFOLLOW` alone
/// does not catch, since it only guards the final component.
///
/// Because this inspects a *handle*, a directory that passes it is pinned: the handle keeps
/// referring to the same inode no matter what happens to the names above it afterwards.
#[cfg(target_os = "linux")]
pub(crate) fn verify_handle_within(file: &File, root: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    let real = std::fs::read_link(&link)
        .map_err(|error| Error::wrap("verifying the opened file's resolved path", error))?;
    if real.starts_with(root) {
        Ok(())
    } else {
        Err(Error::InvalidArgument(
            "the opened file resolves outside the tool's allowed root".into(),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn verify_handle_within(_file: &File, _root: &Path) -> Result<()> {
    Ok(())
}

/// Opens `name` inside the already-resolved directory `parent`, creating it if absent, for
/// writing — without ever following a symlink at the final component.
///
/// On Unix this is deliberately *not* a path-based open. `parent` is opened as a directory
/// handle and the file is then opened relative to that handle with `openat`, so the
/// directory the write lands in is a fixed inode rather than a name that can be re-resolved:
/// renaming or replacing any directory along the path afterwards cannot redirect the write.
/// Both handles are additionally re-confined via [`verify_handle_within`], which is only
/// effective on Linux.
///
/// `O_NOFOLLOW` covers the remaining component. A final segment that is a symlink — whether
/// it points inside the root or outside it — is refused rather than followed, so the path
/// that policy authorized and the object that is written can never diverge.
///
/// `O_NONBLOCK` is set so that a FIFO left at the target fails immediately instead of
/// blocking until a reader appears; the caller still checks the resulting handle is a
/// regular file before writing anything to it.
///
/// `mode` applies only when the file is created; an existing file keeps its own permissions.
#[cfg(unix)]
pub(crate) fn create_within(
    parent: &Path,
    name: &std::ffi::OsStr,
    root: &Path,
    mode: u32,
) -> Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .map_err(|error| Error::wrap("opening the target directory", error))?;
    verify_handle_within(&directory, root)?;

    let raw_name = CString::new(name.as_bytes())
        .map_err(|_| Error::InvalidArgument("path must not contain a NUL byte".into()))?;
    let flags =
        libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `directory` is an open directory descriptor that outlives the call, and
    // `raw_name` is a NUL-terminated C string valid for its duration. The returned
    // descriptor is owned here and immediately handed to `File`.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            raw_name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP) => Error::InvalidArgument(
                "the path's final component is a symlink; this tool never writes through one"
                    .into(),
            ),
            Some(libc::EISDIR) => {
                Error::InvalidArgument("the path names a directory, not a regular file".into())
            }
            _ => Error::wrap("opening the target file for writing", error),
        });
    }
    // SAFETY: `descriptor` is a fresh, owned, valid descriptor returned by `openat` above,
    // and is not used again after this point.
    let file = unsafe { File::from_raw_fd(descriptor) };
    verify_handle_within(&file, root)?;
    Ok(file)
}

/// The portable fallback: a path-based create, with the containment check done on the
/// resolved parent only.
///
/// This platform has no `openat` and no `/proc/self/fd`, so neither the directory-handle
/// pinning nor the symlink refusal above is available; see
/// [`FsWriteTool`](crate::FsWriteTool)'s documentation for what that costs.
#[cfg(not(unix))]
pub(crate) fn create_within(
    parent: &Path,
    name: &std::ffi::OsStr,
    root: &Path,
    _mode: u32,
) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(name))
        .map_err(|error| Error::wrap("opening the target file for writing", error))?;
    verify_handle_within(&file, root)?;
    Ok(file)
}

/// The time remaining under the earlier of `cx`'s deadline and `default_timeout`.
pub(crate) fn remaining_budget(cx: &ExecutionContext, default_timeout: Duration) -> Duration {
    match cx.deadline {
        Some(deadline) => {
            let remaining = deadline
                .to_system_time()
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            remaining.min(default_timeout)
        }
        None => default_timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_with_no_directory_part_resolves_to_the_root_itself() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();

        let (parent, name) = resolve_parent_within(&canonical, "notes.md").unwrap();
        assert_eq!(parent, canonical);
        assert_eq!(name, "notes.md");
    }

    #[test]
    fn a_nested_path_resolves_to_its_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        std::fs::create_dir(canonical.join("src")).unwrap();

        let (parent, name) = resolve_parent_within(&canonical, "src/lib.rs").unwrap();
        assert_eq!(parent, canonical.join("src"));
        assert_eq!(name, "lib.rs");
    }

    #[test]
    fn a_trailing_separator_names_the_last_segment_rather_than_an_empty_one() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();

        let (parent, name) = resolve_parent_within(&canonical, "notes.md/").unwrap();
        assert_eq!(parent, canonical);
        assert_eq!(name, "notes.md");
    }

    #[test]
    fn resolution_refuses_the_same_shapes_for_parents_as_for_whole_paths() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();

        for requested in ["", "/etc/passwd", "../escape", "a/../b", "./a", "a\0b"] {
            assert!(
                resolve_parent_within(&canonical, requested).is_err(),
                "`{requested}` was accepted"
            );
            assert!(
                resolve_within(&canonical, requested).is_err(),
                "`{requested}` was accepted"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn containment_is_component_wise_not_a_string_prefix() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        let sibling = outer.path().join("root-secret");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("token"), "sk-secret").unwrap();
        let root = root.canonicalize().unwrap();

        // Only reachable via a symlink, since `..` is refused syntactically — the point is
        // that the canonical result is rejected on components, not on `starts_with` over
        // the rendered string.
        std::os::unix::fs::symlink(&sibling, root.join("link")).unwrap();
        assert!(resolve_within(&root, "link/token").is_err());
        assert!(resolve_parent_within(&root, "link/token").is_err());
    }
}
