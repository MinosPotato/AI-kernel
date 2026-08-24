//! Where the socket and its token live, and what has to be true of the files.
//!
//! # The filesystem is the first authentication factor
//!
//! A Unix socket is a file, and the kernel's own permission check on that file happens before
//! a single byte of this protocol is read. That check is the strongest one available here, so
//! it is the one this module is careful about:
//!
//! * The directory holding both files is created mode `0700` and is verified on every use —
//!   owned by this account, with nothing for group or other. It protects the *existence* of
//!   the socket and anything a host later puts beside it.
//! * The socket itself is mode `0600`, so even a directory whose mode was loosened later does
//!   not hand the socket to another account.
//! * The token file is mode `0600`, because it is a credential.
//! * A symbolic link in any of those three positions is refused outright rather than
//!   followed. A link is how a path that looks like ours becomes a file that is not, and
//!   there is no legitimate reason for one here.
//!
//! Everything the protocol does on top — peer credentials, the token, the version — is
//! defence in depth behind that. It is worth having precisely because file modes are the kind
//! of thing a deployment, a container image or a hurried `chmod` can get wrong.
//!
//! # Why there is no default outside `XDG_RUNTIME_DIR`
//!
//! `XDG_RUNTIME_DIR` is the one directory the platform promises is private to the account and
//! cleaned up when the session ends, which is exactly what a socket and a per-instance
//! credential want. With none set, this refuses to guess rather than falling back to a
//! world-writable temporary directory: a socket somebody else can create first is a socket
//! somebody else can be listening on, and the failure mode of guessing is a client handing
//! its token, its prompts and its transcripts to whoever won the race. An explicit path is
//! always accepted, so a deployment that knows better says so.

use std::path::{Path, PathBuf};

use aik_core::{Error, Result};

use crate::credentials::current_uid;

/// The mode the directory holding the socket is created with.
pub const RUNTIME_DIRECTORY_MODE: u32 = 0o700;

/// The mode the socket file is given.
pub const SOCKET_FILE_MODE: u32 = 0o600;

/// The directory created under `XDG_RUNTIME_DIR`.
pub const RUNTIME_SUBDIRECTORY: &str = "aik";

/// The socket's file name under that directory.
pub const SOCKET_NAME: &str = "aikd.sock";

/// The environment variable naming a socket, layered under the command line.
pub const SOCKET_ENV: &str = "AIK_SOCKET";

/// The environment variable the default location is derived from.
pub const RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// A socket and the token file beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    socket: PathBuf,
    token: PathBuf,
}

impl Endpoint {
    /// The endpoint at `socket`, with its token file derived from it.
    ///
    /// Derived rather than configured separately so that naming a socket names the whole
    /// endpoint. Two settings would be two things to get wrong, and the interesting way to
    /// get them wrong — a token belonging to a different host than the socket — is one a
    /// client could not detect.
    pub fn at(socket: impl Into<PathBuf>) -> Self {
        let socket = socket.into();
        let token = socket.with_extension("token");
        Self { socket, token }
    }

    /// The endpoint under `XDG_RUNTIME_DIR`, or an explicit one from `AIK_SOCKET`.
    ///
    /// Fails rather than guessing when neither is available; see the module documentation.
    pub fn resolve<I, K, V>(explicit: Option<PathBuf>, vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        if let Some(socket) = explicit {
            return Ok(Self::at(socket));
        }

        let mut from_env: Option<PathBuf> = None;
        let mut runtime_dir: Option<PathBuf> = None;
        for (key, value) in vars {
            match key.as_ref() {
                SOCKET_ENV if !value.as_ref().is_empty() => {
                    from_env = Some(PathBuf::from(value.as_ref()));
                }
                RUNTIME_DIR_ENV if !value.as_ref().is_empty() => {
                    runtime_dir = Some(PathBuf::from(value.as_ref()));
                }
                _ => {}
            }
        }

        if let Some(socket) = from_env {
            return Ok(Self::at(socket));
        }
        let runtime_dir = runtime_dir.ok_or_else(|| {
            Error::config(
                SOCKET_ENV,
                format!(
                    "there is no default location for the socket: neither ${SOCKET_ENV} nor \
                     ${RUNTIME_DIR_ENV} is set, and a socket in a shared temporary directory \
                     could be created by another account before this one gets there"
                ),
            )
        })?;
        Ok(Self::at(
            runtime_dir.join(RUNTIME_SUBDIRECTORY).join(SOCKET_NAME),
        ))
    }

    /// Where the socket is.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Where the token file is.
    pub fn token(&self) -> &Path {
        &self.token
    }

    /// The directory holding both.
    pub fn directory(&self) -> &Path {
        self.socket.parent().unwrap_or(Path::new("."))
    }

    /// Creates the directory if it is missing, and refuses one that is not private.
    ///
    /// Called by the host before binding and by a client before connecting: both sides want
    /// to know that the directory the socket lives in is one only this account can write to,
    /// and the client's check is the one that catches a socket planted by somebody else.
    pub fn prepare_directory(&self) -> Result<()> {
        let directory = self.directory();
        if !directory.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(RUNTIME_DIRECTORY_MODE);
            }
            builder
                .create(directory)
                .map_err(|error| Error::wrap(format!("creating {}", directory.display()), error))?;
        }
        verify_private(directory, "the socket directory")
    }
}

/// Refuses a path that is a symlink, is not owned by this account, or is reachable by others.
///
/// The three checks are one decision — "is this file ours and ours alone?" — and all three
/// are needed. Ownership without the mode check misses a file we own and made readable;
/// the mode check without ownership misses a `0600` file belonging to somebody else, which
/// is precisely what an attacker would leave in a path we are about to trust; and neither
/// catches a symlink, which is how a path that passes both checks names a file that does not.
pub fn verify_private(path: &Path, what: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| Error::wrap(format!("inspecting {}", path.display()), error))?;

    if metadata.file_type().is_symlink() {
        return Err(Error::PermissionDenied(format!(
            "{what} `{}` is a symbolic link; it is refused rather than followed, because a \
             link is how a path that looks like this one names a file that is not",
            path.display(),
        )));
    }

    let owner = metadata.uid();
    let us = current_uid();
    if owner != us {
        return Err(Error::PermissionDenied(format!(
            "{what} `{}` belongs to uid {owner}, not to uid {us}",
            path.display(),
        )));
    }

    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::PermissionDenied(format!(
            "{what} `{}` is mode {mode:04o}, which lets other accounts reach it; \
             it must be private to this account",
            path.display(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_token_is_derived_from_the_socket() {
        let endpoint = Endpoint::at("/run/user/1000/aik/aikd.sock");
        assert_eq!(endpoint.token(), Path::new("/run/user/1000/aik/aikd.token"),);
        assert_eq!(endpoint.directory(), Path::new("/run/user/1000/aik"));
    }

    #[test]
    fn an_explicit_socket_wins_over_every_environment_variable() {
        let endpoint = Endpoint::resolve(
            Some(PathBuf::from("/tmp/explicit.sock")),
            vars(&[
                (SOCKET_ENV, "/tmp/from-env.sock"),
                (RUNTIME_DIR_ENV, "/run/user/1000"),
            ]),
        )
        .expect("resolved");
        assert_eq!(endpoint.socket(), Path::new("/tmp/explicit.sock"));
    }

    #[test]
    fn the_environment_socket_wins_over_the_runtime_directory() {
        let endpoint = Endpoint::resolve(
            None,
            vars(&[
                (SOCKET_ENV, "/tmp/from-env.sock"),
                (RUNTIME_DIR_ENV, "/run/user/1000"),
            ]),
        )
        .expect("resolved");
        assert_eq!(endpoint.socket(), Path::new("/tmp/from-env.sock"));
    }

    #[test]
    fn the_default_lands_under_the_runtime_directory() {
        let endpoint = Endpoint::resolve(None, vars(&[(RUNTIME_DIR_ENV, "/run/user/1000")]))
            .expect("resolved");
        assert_eq!(endpoint.socket(), Path::new("/run/user/1000/aik/aikd.sock"),);
    }

    #[test]
    fn an_empty_environment_variable_names_nothing() {
        // A variable that is set but empty is how a shell says "unset" by accident —
        // `AIK_SOCKET=$MAYBE aik` with `MAYBE` undefined. Reading it as a path would resolve
        // the socket to the empty string, and every later check would be against a path that
        // is not one.
        let endpoint = Endpoint::resolve(
            None,
            vars(&[(SOCKET_ENV, ""), (RUNTIME_DIR_ENV, "/run/user/1000")]),
        )
        .expect("resolved");
        assert_eq!(endpoint.socket(), Path::new("/run/user/1000/aik/aikd.sock"));

        let error = Endpoint::resolve(None, vars(&[(SOCKET_ENV, ""), (RUNTIME_DIR_ENV, "")]))
            .expect_err("an empty runtime directory is no runtime directory");
        assert!(matches!(error, Error::Config { .. }), "{error}");
    }

    #[test]
    fn with_nowhere_private_to_put_a_socket_it_refuses_rather_than_guessing() {
        let error = Endpoint::resolve(None, vars(&[("PATH", "/usr/bin")])).unwrap_err();
        assert!(matches!(error, Error::Config { .. }), "{error}");
        assert!(
            !error.to_string().contains("/tmp"),
            "a shared temporary directory is the one place this must not fall back to: {error}",
        );
    }

    #[test]
    fn preparing_creates_a_private_directory() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let endpoint = Endpoint::at(parent.path().join("run").join("aikd.sock"));

        endpoint.prepare_directory().expect("prepared");

        let mode = std::fs::metadata(endpoint.directory())
            .expect("the directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, RUNTIME_DIRECTORY_MODE);
    }

    #[test]
    fn a_directory_others_can_reach_is_refused_rather_than_tightened() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let directory = parent.path().join("run");
        std::fs::create_dir(&directory).expect("created");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
            .expect("loosened");

        let endpoint = Endpoint::at(directory.join("aikd.sock"));
        let error = endpoint
            .prepare_directory()
            .expect_err("a directory anyone can read must not be used for a socket");
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
        assert!(error.to_string().contains("0755"), "{error}");
    }

    #[test]
    fn a_symbolic_link_is_refused_rather_than_followed() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let real = parent.path().join("real");
        std::fs::create_dir(&real).expect("created");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("tightened");

        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("linked");

        let error = verify_private(&link, "the socket directory")
            .expect_err("a link is how a trusted path names an untrusted file");
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }
}
