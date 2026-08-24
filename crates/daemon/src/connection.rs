//! One client, from its handshake to its last frame.
//!
//! # Three tasks, one connection
//!
//! * The **reader** owns the connection's lifetime. It reads calls, dispatches them, and when
//!   it returns everything else is torn down: the calls in flight are cancelled, the approval
//!   gate is dropped, and the writer's channel closes.
//! * The **writer** owns the socket's write half, and is the only thing that touches it. A
//!   single writer is what makes a frame atomic on the wire — several tasks writing frames
//!   into the same socket would interleave them, and the peer would read the halves of two
//!   messages as one.
//! * The **approval forwarder** exists only for a connection that said somebody is there to
//!   answer. It turns questions from the broker into frames.
//!
//! Each call runs in its own task, so a conversation that is waiting on a model does not stop
//! the client from listing its sessions or cancelling the turn.
//!
//! # Holding a gate is an assertion
//!
//! [`ApprovalBroker`](aik_approval::ApprovalBroker) parks a question only while at least one
//! [`aik_approval::ApprovalGate`] exists; with none, it refuses immediately.
//! This is the only place in the host that obtains one, and it does so exactly when a client
//! said it is interactive — the same rule the terminal frontend follows, where an interactive
//! session subscribes and a one-shot run does not.
//!
//! Two consequences worth stating:
//!
//! * **A connection that goes away stops asserting.** The gate is dropped with the
//!   connection, so a client that was killed mid-question cannot leave the host believing
//!   somebody is still there.
//! * **Questions go to every interactive client.** The broker broadcasts, and the first
//!   answer wins; a later one is told the question was no longer waiting. That is a property
//!   of a host with more than one console attached, and the alternative — routing a question
//!   to the connection whose call caused it — would silently refuse a job's approval whenever
//!   the client that scheduled it had disconnected, which is most of the time.
//!
//! # Bounds
//!
//! A connection may have [`MAX_CALLS_IN_FLIGHT`](crate::settings::MAX_CALLS_IN_FLIGHT) calls
//! outstanding. The excess is refused with an error naming the limit rather than by
//! disconnecting, because the calls already in flight are legitimate work. The outbound
//! channel is bounded too, so a client that stops reading applies backpressure to whatever is
//! producing updates for it rather than growing the host's memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_approval::{ApprovalGate, ApprovalId};
use aik_core::{Error, Result};
use aik_ipc::listener::{Authentication, Peer, authenticate};
use aik_ipc::protocol::{
    Call, PROTOCOL_VERSION, RejectReason, Reply, Request, Response, Welcome, WireError,
};
use aik_ipc::{Token, frame};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::host::{Host, VERSION};

/// How many frames may be queued for one client before producers wait.
const OUTBOUND_CAPACITY: usize = 256;

/// How long a connection's last frames are given to reach a client that has stopped reading.
///
/// Bounded, because a client that is not reading must not be able to hold up the host's
/// shutdown; long enough that one that is reading gets its goodbye.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a cancelled call is given to report that it was cancelled.
///
/// Cancellation is cooperative — nothing here aborts a handler mid-transaction — so this is
/// the moment between "stop" and "you are being stopped whether you noticed or not". Kept
/// short, and together with [`FLUSH_TIMEOUT`] it stays inside the server's own
/// [`SHUTDOWN_TIMEOUT`](crate::server::SHUTDOWN_TIMEOUT).
const CALL_TIMEOUT: Duration = Duration::from_secs(1);

/// Serves one accepted peer until it disconnects or the host shuts down.
///
/// Returns `Ok(false)` for a peer that failed the handshake — a refusal, not an error — and
/// `Ok(true)` for one that was served. An `Err` is a failure of this host's own, such as a
/// socket that could not be written to at all.
pub async fn serve(
    peer: Peer,
    host: Arc<Host>,
    token: Token,
    shutdown: CancellationToken,
    max_calls: usize,
) -> Result<bool> {
    let Peer { mut stream, .. } = peer;

    let hello = match authenticate(&mut stream, &token).await? {
        Authentication::Accepted(hello) => hello,
        Authentication::Rejected(reason, message) => {
            // Best effort: a peer that failed the handshake may already be gone, and a write
            // that fails here is not this host's problem to report.
            let _ = frame::write(&mut stream, &Welcome::Rejected { reason, message }).await;
            return Ok(false);
        }
    };

    // A host on its way down accepts nothing new. Told as a refusal rather than by a closed
    // socket, so a client can say why it could not connect.
    if shutdown.is_cancelled() {
        let _ = frame::write(
            &mut stream,
            &Welcome::Rejected {
                reason: RejectReason::ShuttingDown,
                message: "this host is shutting down".to_owned(),
            },
        )
        .await;
        return Ok(false);
    }

    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);

    // Subscribed before the welcome is sent, so a question asked between the two is queued
    // for this client rather than missed.
    let approvals = hello.interactive.then(|| host.broker().gate().subscribe());
    let gate: Option<ApprovalGate> = approvals.as_ref().map(|stream| stream.gate().clone());

    frame::write(
        &mut writer,
        &Welcome::Accepted {
            protocol: PROTOCOL_VERSION,
            host: format!("aikd {VERSION}"),
            principal: host.principal(),
            interactive: gate.is_some(),
        },
    )
    .await?;

    let connections = host.connected();
    // The name is whatever the client said it was: untrusted text, going into a log. Nothing
    // is decided by it, but a log line is read by a person and sometimes by a parser, and
    // neither should have to cope with a client that named itself with escape sequences.
    let name = describe(&hello.client);
    tracing::info!(
        client = %name,
        interactive = gate.is_some(),
        connections,
        "a client connected",
    );

    let (outbound, inbound) = mpsc::channel::<Response>(OUTBOUND_CAPACITY);
    // Cancelled when this connection ends, for any reason. Every call in flight is a child of
    // it, so a client that disconnects mid-turn cancels its own work rather than leaving it
    // running for nobody.
    let connection = shutdown.child_token();

    // Held apart from the other tasks, and outlives them: the last frames a connection sends
    // are the ones written during its teardown — a call's final answer, the host's goodbye —
    // and a writer aborted alongside everything else would drop exactly those.
    let writing = tokio::spawn({
        let connection = connection.clone();
        async move {
            let mut inbound = inbound;
            while let Some(response) = inbound.recv().await {
                if let Err(error) = frame::write(&mut writer, &response).await {
                    // Almost always the client going away. It can also be a message this end
                    // could not frame at all — one over the frame limit — which is a bug
                    // rather than a disconnection and is worth telling them apart in a log.
                    tracing::debug!(%error, "a connection stopped writing");
                    // Cancelling here is what stops the calls still producing updates for a
                    // client that is no longer reading them.
                    connection.cancel();
                    break;
                }
            }
        }
    });

    let mut tasks = JoinSet::new();

    if let Some(mut approvals) = approvals {
        let outbound = outbound.clone();
        let connection = connection.clone();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = connection.cancelled() => break,
                    pending = approvals.recv() => match pending {
                        Some(pending) => {
                            let response = Response::Approval { pending: Box::new(pending) };
                            if outbound.send(response).await.is_err() {
                                break;
                            }
                        }
                        // The broker closed: the host is stopping, and no further question
                        // can arrive.
                        None => break,
                    },
                }
            }
        });
    }

    let calls: Arc<Mutex<HashMap<u64, CancellationToken>>> = Arc::default();
    let outcome = read_loop(
        &mut reader,
        &host,
        &outbound,
        &calls,
        gate.as_ref(),
        &connection,
        &shutdown,
        max_calls,
        &mut tasks,
    )
    .await;

    // Everything below is the teardown, and the order is the point.
    //
    // The calls are asked to stop and then given a moment to *say* that they stopped: a
    // client that is told its turn was cancelled knows what happened to it, where one whose
    // call simply stopped arriving cannot tell that from a host that hung. Whatever has not
    // finished by then is aborted, because a client must not be able to hold the host open.
    //
    // Only once every call has gone is the connection's own sender dropped, which is what
    // closes the channel, which is what lets the writer flush the goodbye and finish. The
    // writer is aborted last for the same reason it is not in the task set: it is holding the
    // last frames anybody will see.
    connection.cancel();
    drop(gate);
    let reported = tokio::time::timeout(CALL_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if reported.is_err() {
        tracing::warn!("a call did not report its own cancellation in time");
    }
    tasks.shutdown().await;

    drop(outbound);
    if tokio::time::timeout(FLUSH_TIMEOUT, writing).await.is_err() {
        tracing::warn!("a client did not read its last frames in time");
    }
    host.disconnected();

    tracing::info!(client = %name, "a client disconnected");
    outcome.map(|()| true)
}

/// Reads calls until the client goes, the host stops, or the stream breaks.
#[allow(clippy::too_many_arguments)]
async fn read_loop<R>(
    reader: &mut R,
    host: &Arc<Host>,
    outbound: &mpsc::Sender<Response>,
    calls: &Arc<Mutex<HashMap<u64, CancellationToken>>>,
    gate: Option<&ApprovalGate>,
    connection: &CancellationToken,
    shutdown: &CancellationToken,
    max_calls: usize,
    tasks: &mut JoinSet<()>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let call = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                // Best effort, and deliberately not awaited past the channel's capacity: a
                // client that has stopped reading must not be able to hold up the host's
                // shutdown.
                let _ = outbound.try_send(Response::Closing {
                    message: "the host is shutting down".to_owned(),
                });
                return Ok(());
            }
            _ = connection.cancelled() => return Ok(()),
            call = frame::read::<_, Call>(reader) => call,
        };

        let call = match call {
            Ok(Some(call)) => call,
            // The client closed the connection between frames: a clean goodbye.
            Ok(None) => return Ok(()),
            // A frame this host cannot read is the end of the conversation, not a message to
            // skip: the two sides no longer agree about where messages begin, so carrying on
            // would mean parsing the rest of one message as several. There is nowhere to send
            // the diagnosis either, because it belongs to no call.
            Err(error) => {
                tracing::warn!(%error, "a client sent a frame that could not be read");
                return Ok(());
            }
        };

        dispatch(
            call, host, outbound, calls, gate, connection, max_calls, tasks,
        )
        .await;
    }
}

/// Answers one call, or starts a task that will.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    call: Call,
    host: &Arc<Host>,
    outbound: &mpsc::Sender<Response>,
    calls: &Arc<Mutex<HashMap<u64, CancellationToken>>>,
    gate: Option<&ApprovalGate>,
    connection: &CancellationToken,
    max_calls: usize,
    tasks: &mut JoinSet<()>,
) {
    let id = call.id;

    // Answered inline: each is a lookup or a signal, none of them waits on anything, and a
    // cancellation that queued behind the call it is cancelling would never arrive.
    match call.request {
        Request::Cancel { call: target } => {
            let found = calls
                .lock()
                .expect("the in-flight table is never held across a panic")
                .get(&target)
                .cloned();
            let reply = match found {
                Some(token) => {
                    token.cancel();
                    Reply::Ok
                }
                // Not an error: a call that already finished is a call that no longer needs
                // cancelling, and reporting that as a failure would make every race a
                // failure.
                None => Reply::Ok,
            };
            let _ = outbound.send(Response::Done { id, reply }).await;
            return;
        }
        Request::Approve { approval } => {
            let _ = outbound.send(answered(id, gate, &approval, true)).await;
            return;
        }
        Request::Deny { approval } => {
            let _ = outbound.send(answered(id, gate, &approval, false)).await;
            return;
        }
        _ => {}
    }

    let token = connection.child_token();
    // The guard is taken and released without an await in between, deliberately: a lock held
    // across a suspension point is a lock held for as long as a client is slow.
    let admitted = {
        let mut in_flight = calls
            .lock()
            .expect("the in-flight table is never held across a panic");
        if in_flight.len() >= max_calls {
            false
        } else {
            in_flight.insert(id, token.clone());
            true
        }
    };
    if !admitted {
        let error = Error::Unsupported(format!(
            "this connection already has {max_calls} requests in flight, which is the limit"
        ));
        let _ = outbound
            .send(Response::Failed {
                id,
                error: WireError::new(&error),
            })
            .await;
        return;
    }

    let host = host.clone();
    let outbound = outbound.clone();
    let calls = calls.clone();
    let request = call.request;

    tasks.spawn(async move {
        let result = match request {
            Request::Prompt { session, input } => {
                // Updates are forwarded as they arrive rather than collected: a turn that
                // takes a minute should show its first token immediately, which is the whole
                // reason the agent's interface is a stream.
                //
                // The hand-off is unbounded because the producer is synchronous — the agent
                // reports through a plain closure and cannot wait — and it is safe to be
                // because a turn's updates are already bounded by the agent loop's own limits
                // on turns and tool calls. A client that stops reading blocks the pump on the
                // *outbound* channel, which is bounded, so what accumulates is at most one
                // turn's worth rather than everything the host ever produced for it.
                let (updates, mut sink) = mpsc::unbounded_channel();
                let pump = {
                    let outbound = outbound.clone();
                    tokio::spawn(async move {
                        while let Some(update) = sink.recv().await {
                            if outbound
                                .send(Response::Update { id, update })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                };
                let result = host
                    .prompt(session, input, &token, |update| {
                        let _ = updates.send(update);
                    })
                    .await;
                drop(updates);
                let _ = pump.await;
                result
            }
            other => host.handle(other, &token).await,
        };

        calls
            .lock()
            .expect("the in-flight table is never held across a panic")
            .remove(&id);

        let response = match result {
            Ok(reply) => Response::Done { id, reply },
            Err(error) => Response::Failed {
                id,
                error: WireError::new(&error),
            },
        };
        let _ = outbound.send(response).await;
    });
}

/// How many characters of a client's self-declared name are worth logging.
const CLIENT_NAME_LIMIT: usize = 64;

/// Renders a client's name for a log line, keeping nothing that could rewrite one.
///
/// Control characters — which includes newlines, carriage returns and terminal escapes — are
/// replaced rather than escaped, and the result is truncated. A name is display only and
/// grants nothing, so there is no cost to being blunt about it.
fn describe(name: &str) -> String {
    if name.is_empty() {
        return "unnamed".to_owned();
    }
    name.chars()
        .take(CLIENT_NAME_LIMIT)
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

/// The response to an approval this connection answered, or to one it could not.
fn answered(
    id: u64,
    gate: Option<&ApprovalGate>,
    approval: &ApprovalId,
    granted: bool,
) -> Response {
    match answer(gate, approval, granted) {
        Ok(()) => Response::Done {
            id,
            reply: Reply::Ok,
        },
        Err(error) => Response::Failed {
            id,
            error: WireError::new(&error),
        },
    }
}

/// Answers one approval question, or says why this connection cannot.
///
/// A connection that never asserted somebody was there holds no gate, and is told so rather
/// than silently having its answer ignored: a client that believes it approved something,
/// when nothing was approved, is worse than one that gets an error.
fn answer(gate: Option<&ApprovalGate>, approval: &ApprovalId, granted: bool) -> Result<()> {
    let gate = gate.ok_or_else(|| {
        Error::PermissionDenied(
            "this connection did not say a human was present, so it holds no approval gate \
             and cannot answer questions"
                .to_owned(),
        )
    })?;
    if granted {
        gate.approve(approval)
    } else {
        gate.deny(approval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_name_cannot_rewrite_a_log_line() {
        let rendered = describe("aik\r\n2026-01-01 ERROR something that never happened");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(!rendered.contains('\r'), "{rendered}");

        let rendered = describe("aik\u{1b}[2K");
        assert!(!rendered.contains('\u{1b}'), "{rendered}");
    }

    #[test]
    fn a_client_name_is_bounded_and_never_empty() {
        assert_eq!(describe("").as_str(), "unnamed");
        assert_eq!(
            describe(&"x".repeat(1024)).chars().count(),
            CLIENT_NAME_LIMIT,
        );
    }
}
