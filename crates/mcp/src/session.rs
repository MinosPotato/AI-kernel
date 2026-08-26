//! One JSON-RPC conversation, over any pair of streams.
//!
//! Deliberately not tied to a child process. The transport MCP defines over stdio is
//! newline-delimited JSON in both directions, and that is all this module knows: it is given
//! something to read and something to write, and it turns them into request/response calls
//! with ids, timeouts and cancellation. [`crate::process`] is the thin layer that produces
//! those two streams from a program, and the split is what lets every framing and refusal
//! rule below be tested against an in-memory peer rather than against a program that has to
//! exist on the host.
//!
//! Three things go wrong when JSON-RPC is spoken naively to a program, and each is handled
//! here rather than left to the caller:
//!
//! 1. **An unbounded frame.** A line is read into memory before it can be parsed, so a
//!    server that never writes a newline is an out-of-memory kill of the whole kernel. Frames
//!    are capped, and a frame over the cap ends the session rather than being trimmed —
//!    there is no way to resynchronise a stream in the middle of a value nobody has seen the
//!    end of.
//! 2. **A caller that waits forever.** Every call has a wall-clock budget and honours the
//!    [`ExecutionContext`]'s cancellation and deadline. A call that gives up tells the server
//!    so, with `notifications/cancelled`, so a server that is still working can stop.
//! 3. **A server that asks questions.** MCP is bidirectional: a server can ask a client to
//!    sample from its model, to list its filesystem roots, to prompt its user. This client
//!    advertises no capabilities, so every such request is answered with "method not found"
//!    — by id, promptly, rather than ignored, because a server left waiting on an answer is a
//!    server that stops serving.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_core::{Error, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::protocol::{self, Incoming, RequestId};

/// The reading half of a session.
pub(crate) type Source = Box<dyn AsyncRead + Send + Unpin>;

/// The writing half of a session.
pub(crate) type Sink = Box<dyn AsyncWrite + Send + Unpin>;

/// What a pending call is woken with.
type Answer = std::result::Result<Value, Error>;

/// Whether this session can still carry a call, and what is waiting on it.
///
/// One state rather than a map plus a flag, because the two have to change together. A
/// session that ended between a caller checking and that caller registering would otherwise
/// leave the caller in a map nobody drains again — which is not a hang until its own
/// timeout, it *is* the timeout, on a server that already exited and could have said so
/// immediately.
enum Pending {
    /// Calls waiting for an answer, by the id they were sent with.
    Open(HashMap<RequestId, oneshot::Sender<Answer>>),
    /// The session has ended, and this is why.
    Closed(String),
}

/// The state the read loop and the callers share.
struct Shared {
    /// Serialises frame writes, so two concurrent calls cannot interleave halves of a line.
    writer: Mutex<Sink>,
    /// What is waiting, or why nothing can wait any more.
    pending: Mutex<Pending>,
    /// Names the server in every error this session produces.
    label: String,
}

impl Shared {
    /// Writes one frame, followed by the newline that terminates it.
    ///
    /// `serde_json` escapes every control character inside strings, so a serialised value
    /// can never contain the newline that delimits it. Nothing here has to escape anything.
    async fn write(&self, frame: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(frame).map_err(Error::Serialization)?;
        line.push(b'\n');

        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await.map_err(|error| {
            Error::wrap(format!("writing to MCP server `{}`", self.label), error)
        })?;
        writer
            .flush()
            .await
            .map_err(|error| Error::wrap(format!("flushing MCP server `{}`", self.label), error))
    }

    /// Registers a call, or refuses it because the session has already ended.
    async fn register(&self, id: RequestId) -> Result<oneshot::Receiver<Answer>> {
        let mut pending = self.pending.lock().await;
        match &mut *pending {
            Pending::Closed(reason) => Err(Error::other(reason.clone())),
            Pending::Open(waiting) => {
                let (sender, receiver) = oneshot::channel();
                waiting.insert(id, sender);
                Ok(receiver)
            }
        }
    }

    /// Forgets a call that gave up, so a late answer is discarded rather than delivered.
    async fn forget(&self, id: RequestId) {
        if let Pending::Open(waiting) = &mut *self.pending.lock().await {
            waiting.remove(&id);
        }
    }

    /// Takes the sender waiting for `id`, if the session is still open.
    async fn take(&self, id: RequestId) -> Option<oneshot::Sender<Answer>> {
        match &mut *self.pending.lock().await {
            Pending::Open(waiting) => waiting.remove(&id),
            Pending::Closed(_) => None,
        }
    }

    /// Ends the session: wakes every waiting caller with `reason`, and refuses every call
    /// made afterwards with the same one.
    ///
    /// Idempotent, and the first reason wins — the one that actually ended the session,
    /// rather than whichever shutdown path noticed second.
    async fn close(&self, reason: &str) {
        let mut pending = self.pending.lock().await;
        let previous = std::mem::replace(&mut *pending, Pending::Closed(reason.to_owned()));
        if let Pending::Open(waiting) = previous {
            for (_, sender) in waiting {
                let _ = sender.send(Err(Error::other(reason.to_owned())));
            }
        } else {
            *pending = previous;
        }
    }
}

/// A live JSON-RPC conversation with one server.
pub(crate) struct Session {
    shared: Arc<Shared>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("label", &self.shared.label)
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .finish()
    }
}

impl Session {
    /// Starts reading `source` and answering on `sink`.
    pub(crate) fn start(
        label: impl Into<String>,
        source: Source,
        sink: Sink,
        max_frame_bytes: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            writer: Mutex::new(sink),
            pending: Mutex::new(Pending::Open(HashMap::new())),
            label: label.into(),
        });

        let reader = tokio::spawn(read_loop(shared.clone(), source, max_frame_bytes));

        Self {
            shared,
            // Ids start at 1 because JSON-RPC forbids a null id and 0 reads as one in enough
            // implementations to be worth not finding out.
            next_id: AtomicU64::new(1),
            reader,
        }
    }

    /// Sends a request and waits for its answer.
    ///
    /// Gives up after `budget`, or as soon as `cx` is cancelled, whichever comes first. In
    /// both cases the server is told, and the pending entry is removed before returning, so
    /// a late answer is discarded rather than delivered to whoever asks next.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        budget: Duration,
        cx: &ExecutionContext,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Registered before the write, so an answer that arrives between the two has
        // somewhere to go — and refused outright if the server is already gone, rather than
        // waited on for a budget nothing will ever answer within.
        let receiver = self.shared.register(id).await?;

        if let Err(error) = self
            .shared
            .write(&protocol::request(id, method, params))
            .await
        {
            self.shared.forget(id).await;
            return Err(error);
        }

        let outcome = tokio::select! {
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            answer = receiver => match answer {
                Ok(answer) => answer,
                // The read loop ended without waking this caller: the sender was dropped
                // with the whole map, which only happens if the task itself is gone.
                Err(_) => Err(Error::other(format!(
                    "MCP server `{}` stopped answering", self.shared.label
                ))),
            },
            () = tokio::time::sleep(budget) => Err(Error::Timeout(budget)),
        };

        if outcome.is_err() {
            self.shared.forget(id).await;
            // Best effort, and deliberately not awaited for an answer: a cancellation
            // notification is a courtesy to a server that may be working, and a server that
            // has already gone is exactly the case where this call failed.
            let _ = self
                .shared
                .write(&protocol::notification(
                    "notifications/cancelled",
                    serde_json::json!({ "requestId": id }),
                ))
                .await;
        }

        outcome
    }

    /// Sends a one-way message.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.shared
            .write(&protocol::notification(method, params))
            .await
    }

    /// Stops reading and wakes anything still waiting.
    pub(crate) async fn close(&self) {
        self.reader.abort();
        self.shared
            .close(&format!(
                "the session with MCP server `{}` was closed",
                self.shared.label
            ))
            .await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A dropped session must not leave a task reading from a pipe forever. Waking
        // pending callers needs an await and cannot happen here; dropping the map's senders
        // does it instead, which is what the `Err(_)` arm in `call` reads.
        self.reader.abort();
    }
}

/// Reads frames until the stream ends, routing each to whoever is waiting for it.
async fn read_loop(shared: Arc<Shared>, source: Source, max_frame_bytes: usize) {
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();

    let ended = loop {
        line.clear();
        match read_frame(&mut reader, &mut line, max_frame_bytes).await {
            Ok(false) => break format!("MCP server `{}` closed its output", shared.label),
            Err(reason) => break reason,
            Ok(true) => {}
        }

        let frame: Value = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(error) => {
                // Not fatal, and not acted on either. A server that prints a log line to its
                // standard output is violating the transport, but killing the session over
                // it would break working servers to no benefit: an unparseable line is a line
                // that reaches nothing.
                tracing::warn!(
                    server = %shared.label,
                    %error,
                    "ignoring a line from an MCP server that is not JSON"
                );
                continue;
            }
        };

        match protocol::classify(&frame) {
            Ok(Incoming::Response { id, result, error }) => {
                let Some(sender) = shared.take(id).await else {
                    tracing::warn!(
                        server = %shared.label,
                        id,
                        "ignoring an answer from an MCP server to a request nobody made"
                    );
                    continue;
                };
                let answer = match (result, error) {
                    (_, Some(error)) => Err(Error::other(format!(
                        "MCP server `{}` refused the call: {error}",
                        shared.label
                    ))),
                    (Some(result), None) => Ok(result),
                    (None, None) => unreachable!("classify rejects an answer that is neither"),
                };
                let _ = sender.send(answer);
            }
            Ok(Incoming::Request { id, method }) => {
                tracing::debug!(
                    server = %shared.label,
                    %method,
                    "refusing a request from an MCP server: this client advertises no capabilities"
                );
                if let Err(error) = shared.write(&protocol::method_not_found(id, &method)).await {
                    break format!("could not answer MCP server `{}`: {error}", shared.label);
                }
            }
            Ok(Incoming::Notification { method }) => {
                tracing::trace!(server = %shared.label, %method, "ignoring an MCP notification");
            }
            Err(error) => {
                tracing::warn!(
                    server = %shared.label,
                    %error,
                    "ignoring an unroutable frame from an MCP server"
                );
            }
        }
    };

    shared.close(&ended).await;
}

/// Reads one newline-terminated frame into `line`, without the newline.
///
/// Returns `false` at end of stream. A frame larger than `max_bytes` is an error rather than
/// a truncation: the bytes already read are the front half of a value whose end nobody has
/// seen, and continuing would read the rest of it as if it were the next frame.
async fn read_frame<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> std::result::Result<bool, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) => return Err(format!("reading from an MCP server failed: {error}")),
        };

        if available.is_empty() {
            return Ok(!line.is_empty());
        }

        match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                if line.len() + end > max_bytes {
                    return Err(format!(
                        "an MCP server sent a frame larger than the {max_bytes}-byte limit"
                    ));
                }
                line.extend_from_slice(&available[..end]);
                reader.consume(end + 1);
                // A `\r\n` terminator is not the transport MCP specifies, but stripping one
                // costs nothing and turns an otherwise inexplicable parse failure into a
                // working session.
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(true);
            }
            None => {
                let taken = available.len();
                if line.len() + taken > max_bytes {
                    return Err(format!(
                        "an MCP server sent a frame larger than the {max_bytes}-byte limit"
                    ));
                }
                line.extend_from_slice(available);
                reader.consume(taken);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;
    use serde_json::json;
    use tokio::io::DuplexStream;

    /// The peer's own ends of the two streams.
    ///
    /// Kept separate from the [`Session`] so that a test can drive both at once: a call
    /// borrows the session while the peer serving it borrows these.
    struct Peer {
        /// What the session writes, line by line.
        from_client: tokio::io::Lines<BufReader<DuplexStream>>,
        /// What the peer writes back.
        to_client: DuplexStream,
    }

    fn peer(max_frame_bytes: usize) -> (Session, Peer) {
        let (client_reads, peer_writes) = tokio::io::duplex(64 * 1024);
        let (peer_reads, client_writes) = tokio::io::duplex(64 * 1024);
        let session = Session::start(
            "test",
            Box::new(client_reads),
            Box::new(client_writes),
            max_frame_bytes,
        );
        (
            session,
            Peer {
                from_client: BufReader::new(peer_reads).lines(),
                to_client: peer_writes,
            },
        )
    }

    impl Peer {
        async fn next_frame(&mut self) -> Value {
            let line = self
                .from_client
                .next_line()
                .await
                .unwrap()
                .expect("a frame");
            serde_json::from_str(&line).expect("valid JSON")
        }

        async fn send(&mut self, frame: Value) {
            let mut line = serde_json::to_vec(&frame).unwrap();
            line.push(b'\n');
            self.to_client.write_all(&line).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_call_is_answered_by_id() {
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let call = session.call("tools/list", json!({}), Duration::from_secs(5), &cx);
        let serve = async {
            let request = peer.next_frame().await;
            assert_eq!(request["method"], json!("tools/list"));
            assert_eq!(request["jsonrpc"], json!("2.0"));
            peer.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "tools": [] } }))
                .await;
        };

        let (answer, ()) = tokio::join!(call, serve);
        assert_eq!(answer.unwrap(), json!({ "tools": [] }));
    }

    #[tokio::test]
    async fn concurrent_calls_are_matched_to_their_own_answers() {
        // The failure this rules out is two calls in flight and the first answer waking the
        // second caller, which is a tool result attributed to the wrong tool.
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let calls = async {
            tokio::join!(
                session.call("a", json!({}), Duration::from_secs(5), &cx),
                session.call("b", json!({}), Duration::from_secs(5), &cx),
            )
        };

        let serve = async {
            let first = peer.next_frame().await;
            let second = peer.next_frame().await;
            // Answered out of order on purpose.
            peer.send(json!({
                "jsonrpc": "2.0", "id": second["id"], "result": { "who": second["method"] }
            }))
            .await;
            peer.send(json!({
                "jsonrpc": "2.0", "id": first["id"], "result": { "who": first["method"] }
            }))
            .await;
        };

        let ((first, second), ()) = tokio::join!(calls, serve);
        assert_eq!(first.unwrap()["who"], json!("a"));
        assert_eq!(second.unwrap()["who"], json!("b"));
    }

    #[tokio::test]
    async fn a_request_from_a_server_is_refused_by_id() {
        // The failure this rules out is a server asking this kernel's model to generate text
        // — a call the deployment pays for, with a prompt the server wrote — and either
        // getting it or being left waiting forever.
        let (_session, mut peer) = peer(64 * 1024);
        peer.send(json!({
            "jsonrpc": "2.0", "id": 77, "method": "sampling/createMessage",
            "params": { "messages": [] }
        }))
        .await;

        let answer = peer.next_frame().await;
        assert_eq!(answer["id"], json!(77));
        assert_eq!(answer["error"]["code"], json!(protocol::METHOD_NOT_FOUND));
        assert!(answer["result"].is_null());
    }

    #[tokio::test]
    async fn a_call_that_runs_out_of_budget_tells_the_server_and_gives_up() {
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let call = session.call("slow", json!({}), Duration::from_millis(20), &cx);
        let serve = async {
            let request = peer.next_frame().await;
            assert_eq!(request["method"], json!("slow"));
            let cancelled = peer.next_frame().await;
            assert_eq!(cancelled["method"], json!("notifications/cancelled"));
            assert_eq!(cancelled["params"]["requestId"], request["id"]);
        };

        let (answer, ()) = tokio::join!(call, serve);
        assert_eq!(answer.unwrap_err().kind(), ErrorKind::Timeout);
    }

    #[tokio::test]
    async fn a_cancelled_context_stops_the_call() {
        let (session, _peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();
        cx.cancellation.cancel();

        let error = session
            .call("anything", json!({}), Duration::from_secs(30), &cx)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn a_server_that_goes_away_wakes_everyone_waiting() {
        // The failure this rules out is a crashed server leaving an agent turn blocked until
        // its own deadline, with no explanation of why.
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let call = session.call("tools/list", json!({}), Duration::from_secs(30), &cx);
        let serve = async {
            let _ = peer.next_frame().await;
            drop(peer.to_client);
        };

        let (answer, ()) = tokio::join!(call, serve);
        let error = answer.unwrap_err();
        assert!(format!("{error}").contains("closed its output"), "{error}");
    }

    #[tokio::test]
    async fn a_frame_over_the_cap_ends_the_session_rather_than_being_trimmed() {
        let (session, mut peer) = peer(256);
        let cx = ExecutionContext::new();

        let call = session.call("tools/list", json!({}), Duration::from_secs(30), &cx);
        let serve = async {
            let request = peer.next_frame().await;
            peer.send(json!({
                "jsonrpc": "2.0", "id": request["id"], "result": { "blob": "x".repeat(4096) }
            }))
            .await;
        };

        let (answer, ()) = tokio::join!(call, serve);
        let error = answer.unwrap_err();
        assert!(format!("{error}").contains("larger than"), "{error}");
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_ignored_rather_than_fatal() {
        // Servers that print a startup banner to standard output are common enough that
        // refusing to work with them would cost more than it buys: the line reaches nothing
        // either way.
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let call = session.call("tools/list", json!({}), Duration::from_secs(5), &cx);
        let serve = async {
            let request = peer.next_frame().await;
            peer.to_client
                .write_all(b"listening on stdio\n")
                .await
                .unwrap();
            peer.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "ok": true } }))
                .await;
        };

        let (answer, ()) = tokio::join!(call, serve);
        assert_eq!(answer.unwrap(), json!({ "ok": true }));
    }

    #[tokio::test]
    async fn a_call_made_after_the_server_is_gone_fails_at_once() {
        // The failure this rules out is the one the process tests found: a server that exits
        // before a caller registers leaves that caller waiting for its whole budget, because
        // the read loop drained an empty map and never looks again.
        let (session, peer) = peer(64 * 1024);
        drop(peer);
        // Let the read loop notice the closed pipe.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let started = std::time::Instant::now();
        let error = session
            .call(
                "tools/list",
                json!({}),
                Duration::from_secs(30),
                &ExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1), "it waited");
        assert!(format!("{error}").contains("closed its output"), "{error}");
    }

    #[tokio::test]
    async fn an_error_answer_becomes_an_error_naming_the_server() {
        let (session, mut peer) = peer(64 * 1024);
        let cx = ExecutionContext::new();

        let call = session.call("nope", json!({}), Duration::from_secs(5), &cx);
        let serve = async {
            let request = peer.next_frame().await;
            peer.send(json!({
                "jsonrpc": "2.0", "id": request["id"],
                "error": { "code": -32601, "message": "no such method" }
            }))
            .await;
        };

        let (answer, ()) = tokio::join!(call, serve);
        let error = answer.unwrap_err();
        assert!(format!("{error}").contains("no such method"), "{error}");
        assert!(format!("{error}").contains("test"), "{error}");
    }
}
