//! The connecting side.
//!
//! # A client checks the host, not only the other way round
//!
//! Authentication here is mutual in the way that matters locally. The host checks the peer's
//! account and its token; before any of that, the client checks that the socket it is about
//! to hand a token, a prompt and a transcript to is a file *this* account owns, mode `0600`,
//! in a directory this account owns, mode `0700`, with no symbolic link anywhere in the way.
//!
//! Skipping that would make the token an anti-credential: a socket planted by somebody else
//! in a directory this account could reach would receive it, and everything typed afterwards.
//! The check costs one `lstat` per file.
//!
//! # One connection, driven from one place
//!
//! Every method takes `&mut self`, and that is the whole concurrency model. A client sends a
//! call, then reads frames until the answer to it arrives — with approvals and other calls'
//! updates interleaved, which is why [`Client::recv`] hands back whatever came rather than
//! filtering. Anything that needs to send while a read is outstanding, such as answering an
//! approval mid-turn, does so between reads; the frames are small and a read is never
//! blocked on the host doing work, because the host answers questions on the same connection
//! it asked them.

use std::path::Path;

use aik_core::{Error, Result};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::credentials::Token;
use crate::endpoint::{Endpoint, verify_private};
use crate::frame;
use crate::protocol::{
    Call, Hello, PROTOCOL_VERSION, RejectReason, Reply, Request, Response, Welcome,
};

/// What a host said when it accepted the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    /// The host's version string.
    pub host: String,
    /// The principal every call on this connection runs as.
    pub principal: aik_api::permission::Principal,
    /// Whether this connection is holding an approval gate.
    pub interactive: bool,
}

/// A connection to a host process.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    /// Connects to `endpoint`, authenticates, and returns what the host said.
    ///
    /// `interactive` asserts that a human is present to answer approval questions. Claiming
    /// it when nobody is there does not grant anything — an approval still has to be answered
    /// — but it does mean the host parks questions rather than refusing them outright, so a
    /// client that cannot answer should say so and get the faster refusal.
    pub async fn connect(
        endpoint: &Endpoint,
        client: &str,
        interactive: bool,
    ) -> Result<(Self, Connected)> {
        verify_private(endpoint.directory(), "the socket directory")?;
        verify_private(endpoint.socket(), "the socket")?;
        let token = Token::read_from(endpoint.token())?;

        let stream = UnixStream::connect(endpoint.socket())
            .await
            .map_err(|error| {
                Error::wrap(
                    format!(
                        "connecting to the host process at {}",
                        endpoint.socket().display()
                    ),
                    error,
                )
            })?;

        Self::handshake(stream, &token, client, interactive).await
    }

    /// As [`Client::connect`], against an already-open stream and an explicit token.
    ///
    /// Exists for tests and for a caller that obtained the stream some other way; it performs
    /// no filesystem checks, because there is no path to check.
    pub async fn handshake(
        stream: UnixStream,
        token: &Token,
        client: &str,
        interactive: bool,
    ) -> Result<(Self, Connected)> {
        let (reader, mut writer) = stream.into_split();
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            token: token.as_str().to_owned(),
            client: client.to_owned(),
            interactive,
        };
        frame::write(&mut writer, &hello).await?;

        let mut reader = BufReader::new(reader);
        let welcome = frame::read::<_, Welcome>(&mut reader)
            .await?
            .ok_or_else(|| Error::other("the host closed the connection without answering"))?;

        match welcome {
            Welcome::Accepted {
                protocol,
                host,
                principal,
                interactive,
            } => {
                if protocol != PROTOCOL_VERSION {
                    return Err(Error::other(format!(
                        "the host accepted the connection but speaks protocol {protocol}, \
                         not {PROTOCOL_VERSION}"
                    )));
                }
                Ok((
                    Self {
                        reader,
                        writer,
                        next_id: 1,
                    },
                    Connected {
                        host,
                        principal,
                        interactive,
                    },
                ))
            }
            Welcome::Rejected { reason, message } => Err(rejection(reason, &message)),
        }
    }

    /// Sends a request and returns the id its answers will quote.
    pub async fn send(&mut self, request: Request) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        frame::write(&mut self.writer, &Call { id, request }).await?;
        Ok(id)
    }

    /// Waits for the next thing the host sends, or `None` if it closed the connection.
    pub async fn recv(&mut self) -> Result<Option<Response>> {
        frame::read(&mut self.reader).await
    }

    /// Sends one request and waits for its answer, ignoring nothing.
    ///
    /// Anything that arrives in the meantime — an approval question, another call's update —
    /// is handed to `observe` rather than dropped. A caller with nothing to observe passes a
    /// closure that discards, and thereby says so explicitly; silently dropping an approval
    /// question would leave a tool call parked until it expired.
    pub async fn call_observing(
        &mut self,
        request: Request,
        mut observe: impl FnMut(Response),
    ) -> Result<Reply> {
        let id = self.send(request).await?;
        loop {
            let Some(response) = self.recv().await? else {
                return Err(Error::other(
                    "the host closed the connection before answering",
                ));
            };
            match response {
                Response::Done {
                    id: answered,
                    reply,
                } if answered == id => return Ok(reply),
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    return Err(error.into_error());
                }
                Response::Closing { message } => {
                    return Err(Error::other(format!(
                        "the host is shutting down: {message}"
                    )));
                }
                other => observe(other),
            }
        }
    }

    /// Sends one request and waits for its answer, discarding anything else that arrives.
    ///
    /// For a client that holds no approval gate, where nothing else *can* arrive: the host
    /// only pushes questions to connections that asserted somebody is there to answer.
    pub async fn call(&mut self, request: Request) -> Result<Reply> {
        self.call_observing(request, |_| {}).await
    }
}

/// Turns a refusal into an error whose kind says what sort of refusal it was.
fn rejection(reason: RejectReason, message: &str) -> Error {
    let text = format!(
        "the host refused the connection ({}): {message}",
        reason.as_str()
    );
    match reason {
        RejectReason::Unauthenticated => Error::PermissionDenied(text),
        RejectReason::UnsupportedProtocol => Error::Unsupported(text),
        RejectReason::TooManyConnections | RejectReason::ShuttingDown => Error::other(text),
    }
}

/// Whether a host appears to be listening at `endpoint`.
///
/// A cheap probe for a frontend deciding whether to connect or to assemble its own kernel. It
/// authenticates nothing and proves nothing beyond "something accepted a connection here", so
/// it is only ever a hint: the connection that follows does the real checking.
pub fn is_listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listener::{Accepted, Authentication, Listener, authenticate};
    use aik_api::permission::{Principal, PrincipalKind};

    /// A host that accepts one connection and answers whatever the test needs.
    async fn serve(listener: Listener) -> Result<()> {
        let Accepted::Peer(mut peer) = listener.accept().await? else {
            panic!("the test connects as this account");
        };
        match authenticate(&mut peer.stream, listener.token()).await? {
            Authentication::Accepted(hello) => {
                frame::write(
                    &mut peer.stream,
                    &Welcome::Accepted {
                        protocol: PROTOCOL_VERSION,
                        host: "test".to_owned(),
                        principal: Principal::new("assistant", PrincipalKind::Agent)
                            .on_behalf_of("user"),
                        interactive: hello.interactive,
                    },
                )
                .await?;
                // One call, answered with a pong, with an unsolicited frame in front of it.
                let call = frame::read::<_, Call>(&mut peer.stream)
                    .await?
                    .expect("a call");
                frame::write(
                    &mut peer.stream,
                    &Response::Closing {
                        message: "not really".to_owned(),
                    },
                )
                .await?;
                let _ = call;
                Ok(())
            }
            Authentication::Rejected(reason, message) => {
                frame::write(&mut peer.stream, &Welcome::Rejected { reason, message }).await
            }
        }
    }

    #[tokio::test]
    async fn a_refused_handshake_becomes_a_permission_error() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(Endpoint::at(directory.path().join("run").join("aikd.sock")))
            .expect("bound");
        let endpoint = listener.endpoint().clone();
        let host = tokio::spawn(async move { serve(listener).await });

        let wrong = Token::parse(&"0".repeat(64)).expect("a token shape");
        let stream = UnixStream::connect(endpoint.socket())
            .await
            .expect("connected");
        let error = Client::handshake(stream, &wrong, "test", false)
            .await
            .expect_err("a wrong token must not connect");

        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
        host.await.expect("joined").expect("served");
    }

    #[tokio::test]
    async fn a_socket_owned_by_nobody_here_is_never_handed_a_token() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let endpoint = Endpoint::at(directory.path().join("run").join("aikd.sock"));
        endpoint.prepare_directory().expect("prepared");

        // No socket at all is the same class of failure as somebody else's: the client
        // refuses before it reads the token file, which is the property being asserted.
        let error = Client::connect(&endpoint, "test", false)
            .await
            .expect_err("there is nothing to connect to");
        assert!(
            !error.to_string().contains("token file"),
            "the socket is checked before the credential is read: {error}",
        );
    }

    #[tokio::test]
    async fn shutdown_ends_a_call_rather_than_hanging_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let listener = Listener::bind(Endpoint::at(directory.path().join("run").join("aikd.sock")))
            .expect("bound");
        let endpoint = listener.endpoint().clone();
        let host = tokio::spawn(async move { serve(listener).await });

        let (mut client, connected) = Client::connect(&endpoint, "test", true)
            .await
            .expect("connected");
        assert!(connected.interactive);

        let error = client
            .call(Request::Ping)
            .await
            .expect_err("a host that is closing must not leave the call outstanding");
        assert!(error.to_string().contains("shutting down"), "{error}");

        host.await.expect("joined").expect("served");
    }
}
