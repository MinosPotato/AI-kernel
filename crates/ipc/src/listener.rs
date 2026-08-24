//! The listening side: binding a socket only this account can reach, and letting a peer in.
//!
//! # Binding is a refusal, not a takeover
//!
//! A socket file that is already there is either a host that is running or a host that
//! crashed. The two are told apart by connecting to it, and only the second is removed:
//!
//! * A socket that accepts a connection belongs to a live host. Binding over it would leave
//!   two processes serving the same deployment, and the second would fail moments later
//!   anyway when redb refused it the database — but not before the first client had connected
//!   to whichever one won. So this fails immediately and says which path is in use.
//! * A socket that refuses connections is a leftover, and is unlinked. Its ownership and mode
//!   are checked first: removing a file this account does not own is not this program's to do.
//!
//! # What accepting checks
//!
//! The peer's account, from the kernel, before anything the peer wrote is parsed. Everything
//! else — the token, the protocol version — happens in [`authenticate`], on bytes the peer
//! chose, and is deliberately downstream of the one check the peer cannot influence.

use std::path::Path;

use aik_core::{Error, Result};
use tokio::net::{UnixListener, UnixStream};

use crate::credentials::{Token, current_uid};
use crate::endpoint::{Endpoint, SOCKET_FILE_MODE, verify_private};
use crate::frame;
use crate::protocol::{HANDSHAKE_TIMEOUT, Hello, PROTOCOL_VERSION, RejectReason};

/// A bound socket, its per-instance token, and the files both live in.
///
/// Dropping it removes the socket and the token file, so a host that exits — cleanly, by
/// panic, or by an error on the way up — leaves nothing behind for the next one to have to
/// reason about. A token that outlived its host would be a credential for a process that no
/// longer exists.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    endpoint: Endpoint,
    token: Token,
}

/// What accepting produced.
#[derive(Debug)]
pub enum Accepted {
    /// A peer running as this account.
    Peer(Peer),
    /// A peer that failed the account check and has been disconnected.
    ///
    /// Reported rather than swallowed so a host can say so out loud: another account reaching
    /// this socket at all means the directory's mode is not what it should be, which is worth
    /// an operator's attention even though the connection was refused.
    Refused {
        /// The account that tried.
        uid: u32,
    },
}

/// An accepted connection, and who is on the other end of it.
#[derive(Debug)]
pub struct Peer {
    /// The connection.
    pub stream: UnixStream,
    /// The peer's account, from the operating system.
    pub uid: u32,
    /// The peer's process, if the platform reported one. For diagnostics only.
    pub pid: Option<i32>,
}

impl Listener {
    /// Prepares the directory, binds the socket and writes a fresh token beside it.
    pub fn bind(endpoint: Endpoint) -> Result<Self> {
        endpoint.prepare_directory()?;
        clear_stale_socket(endpoint.socket())?;

        // Written *before* the socket exists, deliberately. Binding makes the socket
        // connectable immediately, so a token written afterwards leaves a window in which a
        // client can reach the host and find no credential to present — which is a race that
        // shows up as an authentication failure and reads like a security problem.
        let token = Token::generate()?;
        token.write_to(endpoint.token())?;

        let listener = bind_privately(endpoint.socket()).inspect_err(|_| {
            // The token belongs to a host that does not exist; leaving it would mean the next
            // reader picking up a credential for nothing.
            let _ = std::fs::remove_file(endpoint.token());
        })?;

        Ok(Self {
            listener,
            endpoint,
            token,
        })
    }

    /// Where this listener is.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The token a client has to present.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Waits for a peer, checking its account before returning it.
    pub async fn accept(&self) -> Result<Accepted> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|error| Error::wrap("accepting a connection", error))?;

        // A platform that cannot report peer credentials cannot be trusted to have refused
        // anybody, so this fails closed rather than proceeding on the file mode alone.
        let credentials = stream
            .peer_cred()
            .map_err(|error| Error::wrap("reading the peer's credentials", error))?;

        let uid = credentials.uid();
        if uid != current_uid() {
            // Dropped, which closes it. Nothing is written back: a peer that is not this
            // account gets no protocol at all, not even a refusal it could time.
            drop(stream);
            return Ok(Accepted::Refused { uid });
        }

        Ok(Accepted::Peer(Peer {
            stream,
            uid,
            pid: credentials.pid(),
        }))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.endpoint.socket());
        let _ = std::fs::remove_file(self.endpoint.token());
    }
}

/// What the handshake decided.
#[derive(Debug)]
pub enum Authentication {
    /// The peer is authenticated; this is what it asked for.
    Accepted(Hello),
    /// The peer is not, and this is what to tell it.
    Rejected(RejectReason, String),
}

/// Reads a peer's [`Hello`] and decides whether it may proceed.
///
/// The order is deliberate: the token first, the protocol version second. A peer that has not
/// authenticated learns nothing about this host, not even which versions it speaks — and a
/// malformed handshake is indistinguishable from a wrong token, because both are simply
/// "unauthenticated".
///
/// A peer that says nothing at all is timed out. An unauthenticated connection costs a
/// connection slot, and one that could hold it indefinitely would be all an attacker needed
/// to keep every other client out.
pub async fn authenticate(stream: &mut UnixStream, token: &Token) -> Result<Authentication> {
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::read::<_, Hello>(stream)).await;

    let hello = match hello {
        Err(_elapsed) => {
            return Ok(Authentication::Rejected(
                RejectReason::Unauthenticated,
                format!(
                    "no handshake within {} seconds",
                    HANDSHAKE_TIMEOUT.as_secs()
                ),
            ));
        }
        // A frame that will not parse is not a protocol error worth reporting in detail; it
        // is a peer that did not present a credential, and is answered as one.
        Ok(Err(_)) | Ok(Ok(None)) => {
            return Ok(Authentication::Rejected(
                RejectReason::Unauthenticated,
                "the handshake was not a handshake".to_owned(),
            ));
        }
        Ok(Ok(Some(hello))) => hello,
    };

    if !token.matches(&hello.token) {
        return Ok(Authentication::Rejected(
            RejectReason::Unauthenticated,
            "the token presented is not this host's".to_owned(),
        ));
    }

    if hello.protocol != PROTOCOL_VERSION {
        return Ok(Authentication::Rejected(
            RejectReason::UnsupportedProtocol,
            format!(
                "this host speaks protocol {PROTOCOL_VERSION}; the client speaks {}",
                hello.protocol
            ),
        ));
    }

    Ok(Authentication::Accepted(hello))
}

/// Binds a socket that is mode `0600` from the moment it is reachable.
///
/// The obvious sequence — bind, then `chmod` — leaves a window in which the socket exists at
/// whatever the process umask allowed, and a client that connects during it finds a socket it
/// is right to refuse. The directory is `0700`, so the window is not a way in for another
/// account; it is a way for a *correct* client to fail, intermittently, for a reason that
/// reads exactly like an attack.
///
/// So the socket is bound under a temporary name in the same private directory, tightened
/// there, and renamed into place. The rename is atomic and the listening socket keeps working
/// under its new name, so the path a client connects to never exists at any other mode.
fn bind_privately(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = path.parent().unwrap_or(Path::new("."));
    let staged = directory.join(format!(
        ".{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("socket"),
        std::process::id(),
    ));
    // A leftover from a previous crash would make the bind fail; removing it is safe because
    // the directory is private to this account and the name carries this process's own pid.
    let _ = std::fs::remove_file(&staged);

    let listener = UnixListener::bind(&staged)
        .map_err(|error| Error::wrap(format!("binding the socket {}", staged.display()), error))?;

    let installed =
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(SOCKET_FILE_MODE))
            .and_then(|()| std::fs::rename(&staged, path));

    match installed {
        Ok(()) => Ok(listener),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(Error::wrap(
                format!("installing the socket {}", path.display()),
                error,
            ))
        }
    }
}

/// What a refused connection to an existing socket path means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leftover {
    /// Nothing is listening: the file belongs to a host that is gone.
    Stale,
    /// The path is not there any more; there is nothing to clear.
    Gone,
    /// Something else went wrong, and it is not safe to assume either of the above.
    Unknown,
}

/// Classifies the failure to connect to a socket that this account owns.
///
/// Extracted from [`clear_stale_socket`] because it is the whole of the decision to *delete*
/// somebody's socket, and it is otherwise reachable only by arranging rare `connect` failures.
/// The distinctions matter:
///
/// * A connection **refused** is the definitive signal: a socket file exists, the kernel has
///   no listener bound to it, and nothing else produces that. It is a leftover.
/// * **Not found** means the path went away between the check and the connection. There is
///   nothing to remove and nothing wrong; a fresh bind will simply create it.
/// * Anything else — the process out of file descriptors, a permission error, an interrupted
///   call — says nothing about whether a host is running. Treating it as staleness would mean
///   deleting the socket of a live host because this process happened to be out of resources,
///   so it fails instead.
fn classify(kind: std::io::ErrorKind) -> Leftover {
    match kind {
        std::io::ErrorKind::ConnectionRefused => Leftover::Stale,
        std::io::ErrorKind::NotFound => Leftover::Gone,
        _ => Leftover::Unknown,
    }
}

/// Removes a socket left by a host that is gone, and refuses to displace one that is not.
fn clear_stale_socket(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        // Nothing there at all, which is the ordinary case.
        return Ok(());
    };

    // Ownership and mode first: unlinking a path this account does not own is not something
    // to do on the way to binding a socket.
    verify_private(path, "the socket")?;

    // And it has to actually be a socket. A regular file here is not a leftover host, it is
    // somebody's data at a path that was configured wrongly, and removing it would be this
    // program destroying a file it was never asked to touch.
    {
        use std::os::unix::fs::FileTypeExt as _;
        if !metadata.file_type().is_socket() {
            return Err(Error::AlreadyExists {
                kind: "file",
                id: format!(
                    "{} exists and is not a socket; it is refused rather than removed",
                    path.display()
                ),
            });
        }
    }

    let error = match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => {
            return Err(Error::AlreadyExists {
                kind: "host process",
                id: format!(
                    "{} is already being served; stop that host before starting another",
                    path.display()
                ),
            });
        }
        Err(error) => error,
    };

    match classify(error.kind()) {
        Leftover::Stale => std::fs::remove_file(path).map_err(|error| {
            Error::wrap(
                format!("removing the stale socket {}", path.display()),
                error,
            )
        }),
        Leftover::Gone => Ok(()),
        Leftover::Unknown => Err(Error::wrap(
            format!(
                "deciding whether {} belongs to a running host",
                path.display()
            ),
            error,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn endpoint(directory: &tempfile::TempDir) -> Endpoint {
        Endpoint::at(directory.path().join("run").join("aikd.sock"))
    }

    #[tokio::test]
    async fn binding_creates_a_private_socket_and_a_private_token() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(endpoint(&directory)).expect("bound");

        let socket_mode = std::fs::metadata(listener.endpoint().socket())
            .expect("the socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(socket_mode, SOCKET_FILE_MODE);

        let token_mode = std::fs::metadata(listener.endpoint().token())
            .expect("the token file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(token_mode, 0o600);
    }

    #[tokio::test]
    async fn a_second_host_on_the_same_socket_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let first = Listener::bind(endpoint(&directory)).expect("bound");

        let error = Listener::bind(endpoint(&directory))
            .expect_err("two hosts must not serve one deployment");
        assert_eq!(error.kind(), aik_core::ErrorKind::Conflict);
        drop(first);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_dead_host_is_replaced() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let endpoint = endpoint(&directory);
        endpoint.prepare_directory().expect("prepared");

        // A crashed host: the socket file is still on disk, and nothing is listening on it.
        // `std`'s listener does not unlink on drop, which is exactly the leftover being
        // simulated; the mode is the one a host would have left.
        let dead = std::os::unix::net::UnixListener::bind(endpoint.socket()).expect("bound");
        drop(dead);
        std::fs::set_permissions(
            endpoint.socket(),
            std::fs::Permissions::from_mode(SOCKET_FILE_MODE),
        )
        .expect("tightened");
        assert!(
            endpoint.socket().exists(),
            "the socket has to still be there"
        );

        let second = Listener::bind(endpoint).expect("a stale socket is replaceable");
        assert!(second.endpoint().socket().exists());
    }

    #[test]
    fn only_a_refused_connection_means_a_socket_may_be_deleted() {
        use std::io::ErrorKind;

        assert_eq!(classify(ErrorKind::ConnectionRefused), Leftover::Stale);
        assert_eq!(classify(ErrorKind::NotFound), Leftover::Gone);

        // Everything else says nothing about whether a host is running, and must not be read
        // as permission to delete its socket.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::Other,
        ] {
            assert_eq!(
                classify(kind),
                Leftover::Unknown,
                "{kind:?} must not be read as a leftover",
            );
        }
    }

    #[tokio::test]
    async fn something_that_is_not_a_socket_is_refused_rather_than_deleted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let endpoint = endpoint(&directory);
        endpoint.prepare_directory().expect("prepared");

        // A configuration mistake: the path names somebody's file. Removing it would be this
        // program destroying data it was never asked to touch.
        std::fs::write(endpoint.socket(), b"not a socket").expect("written");
        std::fs::set_permissions(endpoint.socket(), std::fs::Permissions::from_mode(0o600))
            .expect("tightened");

        let path = endpoint.socket().to_path_buf();
        let error =
            Listener::bind(endpoint).expect_err("a regular file must not be silently removed");
        assert_eq!(error.kind(), aik_core::ErrorKind::Conflict, "{error}");
        assert_eq!(
            std::fs::read(&path).expect("still there"),
            b"not a socket",
            "it is refused, not removed",
        );
    }

    #[tokio::test]
    async fn a_stale_socket_this_account_cannot_vouch_for_is_refused_rather_than_removed() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let endpoint = endpoint(&directory);
        endpoint.prepare_directory().expect("prepared");

        let dead = std::os::unix::net::UnixListener::bind(endpoint.socket()).expect("bound");
        drop(dead);
        std::fs::set_permissions(endpoint.socket(), std::fs::Permissions::from_mode(0o666))
            .expect("loosened");

        let path = endpoint.socket().to_path_buf();
        let error = Listener::bind(endpoint)
            .expect_err("a socket anyone could have created must not be silently replaced");
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
        assert!(path.exists(), "it is refused, not removed");
    }

    #[tokio::test]
    async fn dropping_the_listener_removes_both_files() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(endpoint(&directory)).expect("bound");
        let socket = listener.endpoint().socket().to_path_buf();
        let token = listener.endpoint().token().to_path_buf();

        drop(listener);

        assert!(!socket.exists(), "a socket must not outlive its host");
        assert!(
            !token.exists(),
            "a credential must not outlive the host it authenticates",
        );
    }

    #[tokio::test]
    async fn a_wrong_token_and_a_malformed_handshake_are_the_same_refusal() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(endpoint(&directory)).expect("bound");
        let path = listener.endpoint().socket().to_path_buf();

        for (label, hello) in [
            (
                "a wrong token",
                serde_json::json!({
                    "protocol": PROTOCOL_VERSION,
                    "token": "0".repeat(64),
                }),
            ),
            ("no token at all", serde_json::json!({ "protocol": 1 })),
            ("not a handshake", serde_json::json!({ "hello": true })),
        ] {
            let client = tokio::spawn({
                let path = path.clone();
                async move {
                    let mut stream = UnixStream::connect(&path).await.expect("connected");
                    frame::write(&mut stream, &hello).await.expect("written");
                }
            });

            let Accepted::Peer(mut peer) = listener.accept().await.expect("accepted") else {
                panic!("the test connects as this account");
            };
            let outcome = authenticate(&mut peer.stream, listener.token())
                .await
                .expect("decided");
            match outcome {
                Authentication::Rejected(RejectReason::Unauthenticated, _) => {}
                other => panic!("{label} must be unauthenticated, got {other:?}"),
            }
            client.await.expect("the client finished");
        }
    }

    #[tokio::test]
    async fn a_version_mismatch_is_reported_only_after_the_token_is_accepted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(endpoint(&directory)).expect("bound");
        let path = listener.endpoint().socket().to_path_buf();
        let token = listener.token().as_str().to_owned();

        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(&path).await.expect("connected");
            let hello = Hello {
                protocol: PROTOCOL_VERSION + 1,
                token,
                client: "test".to_owned(),
                interactive: false,
            };
            frame::write(&mut stream, &hello).await.expect("written");
        });

        let Accepted::Peer(mut peer) = listener.accept().await.expect("accepted") else {
            panic!("the test connects as this account");
        };
        match authenticate(&mut peer.stream, listener.token())
            .await
            .expect("decided")
        {
            Authentication::Rejected(RejectReason::UnsupportedProtocol, message) => {
                assert!(message.contains("protocol"), "{message}");
            }
            other => panic!("expected a version refusal, got {other:?}"),
        }
        client.await.expect("the client finished");
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_says_nothing_is_timed_out_rather_than_holding_a_slot() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(endpoint(&directory)).expect("bound");
        let path = listener.endpoint().socket().to_path_buf();

        let _silent = UnixStream::connect(&path).await.expect("connected");
        let Accepted::Peer(mut peer) = listener.accept().await.expect("accepted") else {
            panic!("the test connects as this account");
        };

        // Time is paused and auto-advances while the runtime is idle, so the handshake
        // deadline arrives without the test waiting out ten real seconds.
        let token = Token::generate().expect("generated");
        match authenticate(&mut peer.stream, &token)
            .await
            .expect("decided")
        {
            Authentication::Rejected(RejectReason::Unauthenticated, message) => {
                assert!(message.contains("handshake"), "{message}");
            }
            other => panic!("expected a timeout refusal, got {other:?}"),
        }
    }
}
