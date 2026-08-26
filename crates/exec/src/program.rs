//! Deciding which program a request names, and where on the host it actually is.
//!
//! Two separate questions, answered in this order and never merged:
//!
//! 1. **Is this a program name at all?** [`validate_name`] accepts a bare file name and
//!    nothing else. A request never names a path, so there is no path for a `..`, a symlink
//!    or a leading `/` to be smuggled through, and no resolution step that could be
//!    influenced by where the host happens to be standing.
//! 2. **Is this program one this tool runs?** The allowlist. A name that is not on it is
//!    refused before anything is resolved, before policy is asked, and before any part of
//!    the host filesystem is touched.
//!
//! Only then is the name turned into an absolute path, by scanning a *configured* search
//! path — never the process environment's `PATH`, which is inherited from whoever started
//! the kernel and is therefore attacker-influenced in exactly the deployments that matter.
//!
//! # Why this module is public
//!
//! This crate is not the only one that starts host code from a configured name:
//! [`aik-mcp`](../aik_mcp/index.html) starts a tool server the same way. Two copies of
//! "which file does the name `uvx` mean?" would be two answers that agree today, and the
//! first divergence anybody noticed would be one of them resolving on the inherited `PATH`.
//! So there is one answer, here, and the other crate calls it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use aik_core::{Error, Result};

/// The characters a program name may contain.
///
/// Deliberately narrower than what a filesystem accepts. Every name that could plausibly be
/// wanted (`git`, `rg`, `python3`, `cargo-fmt`) fits, and everything that makes a name
/// ambiguous when it is later shown to a human — whitespace, quotes, control characters,
/// anything non-ASCII that could render as a different name than it is — does not.
fn is_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+')
}

/// Accepts a bare program name, or explains why this is not one.
///
/// Rejected: anything empty, anything containing a path separator or a NUL, `.` and `..`,
/// a leading `-` (which every argument parser downstream would read as a flag rather than a
/// program), and anything outside the accepted character set.
pub fn validate_name(raw: &str) -> Result<&str> {
    let refuse = |why: &str| {
        Err(Error::InvalidArgument(format!(
            "`{raw}` is not a usable program name: {why}"
        )))
    };

    if raw.is_empty() {
        return refuse("it is empty");
    }
    if raw == "." || raw == ".." {
        return refuse("it names a directory, not a program");
    }
    if raw.starts_with('-') {
        return refuse("it starts with `-`");
    }
    if !raw.bytes().all(is_name_char) {
        return refuse(
            "only ASCII letters, digits, `_`, `-`, `.` and `+` are allowed, and a program is \
             named, never given as a path",
        );
    }
    Ok(raw)
}

/// Finds `name` in `search_path`, returning the absolute path that will be executed.
///
/// The first executable regular file wins, which is what a shell would do, so a deployment
/// that puts a wrapper directory first in its configured search path gets the wrapper. The
/// difference from a shell is that the search path is the deployment's, not the
/// environment's.
///
/// The returned path is canonical: symlinks are resolved once, here, so what is recorded in
/// an audit trail and what is handed to the sandbox are the same real file. A name that
/// resolves to a symlink pointing outside the search path is *not* refused — `/usr/bin/vi`
/// legitimately points wherever the distribution decided — because the allowlist, not the
/// location, is what says whether this program may run.
pub fn resolve(name: &str, search_path: &[PathBuf]) -> Result<PathBuf> {
    for directory in search_path {
        let candidate = directory.join(name);
        if !is_executable_file(&candidate) {
            continue;
        }
        return std::fs::canonicalize(&candidate)
            .map_err(|error| Error::wrap(format!("resolving `{}`", candidate.display()), error));
    }
    Err(Error::not_found("program", name))
}

/// Whether `path` is a regular file with an execute bit set for somebody.
///
/// The mode check is a fast, honest filter and not an authorization decision: the kernel
/// decides at `execve` whether *this* process may run it, and a file that passes here can
/// still be refused there. What it buys is that a directory or a device node named `git`
/// does not shadow the real one and turn into a confusing failure later.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

/// Splits a configured `PATH`-style string into directories.
///
/// Relative entries are dropped rather than resolved: a search path entry that depends on
/// the working directory means the program a name resolves to changes with where the process
/// was started, which is the one property a program allowlist must not have. An empty entry
/// — what a stray `:` produces, and what a shell reads as "the current directory" — is
/// dropped for the same reason.
pub fn parse_search_path(raw: &str) -> Vec<PathBuf> {
    raw.split(':')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .filter(|entry| entry.is_absolute())
        .collect()
}

/// Renders `path` for a message, without inventing a lossless form it does not have.
pub(crate) fn display(path: &OsStr) -> String {
    Path::new(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    #[test]
    fn a_plain_name_is_accepted() {
        for name in ["git", "python3", "cargo-fmt", "g++", "a.out", "_x"] {
            assert_eq!(validate_name(name).unwrap(), name, "{name}");
        }
    }

    #[test]
    fn a_path_is_never_a_program_name() {
        for raw in [
            "/bin/sh",
            "./sh",
            "../sh",
            "bin/sh",
            "..",
            ".",
            "sh\0",
            "",
            "-rf",
            "git status",
            "gi\u{0074}\u{0301}",
        ] {
            let error = validate_name(raw).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{raw:?}");
        }
    }

    #[test]
    fn resolution_scans_the_configured_path_in_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_executable(first.path(), "tool");
        write_executable(second.path(), "tool");

        let search = vec![second.path().to_owned(), first.path().to_owned()];
        let found = resolve("tool", &search).unwrap();
        assert_eq!(found, second.path().canonicalize().unwrap().join("tool"));
    }

    #[test]
    fn a_directory_does_not_shadow_a_program() {
        let shadow = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir(shadow.path().join("tool")).unwrap();
        write_executable(real.path(), "tool");

        let search = vec![shadow.path().to_owned(), real.path().to_owned()];
        let found = resolve("tool", &search).unwrap();
        assert!(found.starts_with(real.path().canonicalize().unwrap()));
    }

    #[test]
    fn a_non_executable_file_is_not_a_program() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("tool"), "#!/bin/sh\n").unwrap();

        let error = resolve("tool", &[directory.path().to_owned()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn relative_and_empty_search_path_entries_are_dropped() {
        let parsed = parse_search_path("/usr/bin::relative:/bin");
        assert_eq!(
            parsed,
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
    }

    #[cfg(unix)]
    fn write_executable(directory: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn write_executable(directory: &Path, name: &str) {
        std::fs::write(directory.join(name), "").unwrap();
    }
}
