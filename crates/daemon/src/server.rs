//! Accepting clients, and stopping.
//!
//! # What the accept loop is responsible for
//!
//! Bounding. Everything else about a connection belongs to
//! [`crate::connection::serve`]; what happens here is the decision to serve
//! one at all:
//!
//! * A peer that is not this account is refused by [`Listener::accept`] before a byte it wrote
//!   is read, and is reported. Another account reaching this socket means the directory's mode
//!   is not what it should be, which is worth an operator's attention even though nothing was
//!   served.
//! * A peer arriving when the host is already serving its limit is told so and disconnected.
//!   The limit counts tasks, including connections still in their handshake, so a client in a
//!   restart loop cannot accumulate half-open connections until the host runs out of file
//!   descriptors — which would take the schedule down with it.
//!
//! # Stopping
//!
//! One token, cancelled by a signal or by whatever else asked. Every connection holds a child
//! of it, so cancelling reaches each reader, which tells its client the host is closing,
//! cancels the calls that connection had in flight, and returns.
//!
//! Connections are then waited for, briefly. A client that has stopped reading its socket must
//! not be able to hold the host open, so the wait is bounded and whatever is left is aborted.
//! Only then does the kernel shut down, which is what closes the broker — refusing every
//! approval still parked — and stops the scheduler.

use std::sync::Arc;
use std::time::Duration;

use aik_core::Result;
use aik_ipc::frame;
use aik_ipc::listener::{Accepted, Listener};
use aik_ipc::protocol::{RejectReason, Welcome};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::connection;
use crate::host::Host;
use crate::settings::MAX_CALLS_IN_FLIGHT;

/// How long connections are given to finish once the host is stopping.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a refusal may take to write before the peer is simply dropped.
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(2);

/// The accept loop over one bound socket.
#[derive(Debug)]
pub struct Server {
    host: Arc<Host>,
    listener: Listener,
    max_connections: usize,
    shutdown: CancellationToken,
}

impl Server {
    /// Serves `host` on `listener` until `shutdown` is cancelled.
    pub fn new(
        host: Arc<Host>,
        listener: Listener,
        max_connections: usize,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            host,
            listener,
            max_connections,
            shutdown,
        }
    }

    /// Accepts clients until the host is asked to stop, then waits for them.
    ///
    /// Consumes the server so that the listener — and with it the socket and the token file,
    /// which its `Drop` removes — cannot outlive the loop that was serving them.
    pub async fn run(self) -> Result<()> {
        let mut connections: JoinSet<()> = JoinSet::new();

        loop {
            // Finished connections are reaped every time round, so the set's length is the
            // number actually being served and can be used as the bound.
            while connections.try_join_next().is_some() {}

            let accepted = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };

            match accepted {
                Ok(Accepted::Peer(peer)) => {
                    // Reaped again before refusing anybody: a connection that has already
                    // ended must not cost the next client its slot, and between the top of
                    // the loop and here is exactly where one is most likely to have.
                    while connections.try_join_next().is_some() {}

                    if connections.len() >= self.max_connections {
                        let limit = self.max_connections;
                        connections.spawn(async move {
                            refuse(peer, limit).await;
                        });
                        continue;
                    }

                    let host = self.host.clone();
                    let token = self.listener.token().clone();
                    let shutdown = self.shutdown.clone();
                    connections.spawn(async move {
                        match connection::serve(peer, host, token, shutdown, MAX_CALLS_IN_FLIGHT)
                            .await
                        {
                            Ok(true) | Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "a connection ended in a failure");
                            }
                        }
                    });
                }
                Ok(Accepted::Refused { uid }) => {
                    tracing::warn!(
                        uid,
                        socket = %self.listener.endpoint().socket().display(),
                        "refused a connection from another account; check the socket \
                         directory's permissions",
                    );
                }
                // Accepting failed. Not fatal on its own — a peer that vanished between the
                // connection and the accept produces one of these — so it is reported and the
                // loop carries on rather than taking the host down with it.
                Err(error) => {
                    tracing::warn!(%error, "failed to accept a connection");
                }
            }
        }

        // Waited for here rather than in a helper, and waited for at all rather than simply
        // dropped: a connection abandoned mid-teardown is one whose last frames — a cancelled
        // call's answer, the host's goodbye — are discarded, and whose client cannot tell that
        // from a host that hung. The wait is bounded, because a client that has stopped
        // reading must not be able to hold the host open.
        // No guard on there being any: joining an empty set completes at once, and a guard
        // would only be a second way for the wait to be skipped.
        let waited = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
            while connections.join_next().await.is_some() {}
        })
        .await;

        if waited.is_err() {
            tracing::warn!(
                remaining = connections.len(),
                "some clients did not disconnect in time; closing anyway",
            );
            connections.shutdown().await;
        }

        Ok(())
    }
}

/// Tells a peer the host is full, without serving it.
///
/// Written before the handshake is read, which is the one thing this host says to a peer it
/// has not authenticated. It says only "full", to a peer already known to be this account, and
/// the alternative — closing the socket silently — would leave a client reporting a bare
/// disconnection for a condition that is temporary and worth naming.
async fn refuse(peer: aik_ipc::listener::Peer, limit: usize) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = peer.stream;
    let rejection = Welcome::Rejected {
        reason: RejectReason::TooManyConnections,
        message: format!("this host is already serving {limit} clients"),
    };

    let closed = tokio::time::timeout(REFUSAL_TIMEOUT, async {
        frame::write(&mut stream, &rejection).await?;
        // Half-closed rather than dropped, and then drained. A client writes its handshake
        // and only then reads, so a host that closed the socket outright here would reset the
        // connection under a write still in flight — and the client would report a broken
        // pipe instead of the refusal it was just sent. Shutting down the write side sends
        // the end-of-stream the client needs while leaving the read side able to absorb the
        // handshake it is mid-way through sending.
        stream
            .shutdown()
            .await
            .map_err(|error| aik_core::Error::wrap("closing a refused connection", error))?;
        // Bounded and read to the end, so the handshake the client is mid-way through
        // sending is absorbed rather than reset, and a client that keeps talking cannot make
        // this end read forever.
        let mut discarded = Vec::new();
        let _ = (&mut stream).take(1024).read_to_end(&mut discarded).await;
        Ok::<(), aik_core::Error>(())
    })
    .await;

    if closed.is_err() {
        tracing::warn!("a refused client did not close its connection in time");
    }
    tracing::warn!(limit, "refused a client: the connection limit is reached");
}
