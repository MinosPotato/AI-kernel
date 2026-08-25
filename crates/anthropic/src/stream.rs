//! Turns the Messages API's server-sent event stream into [`CompletionChunk`]s.
//!
//! Two things make this more than a line parser.
//!
//! **Tool arguments arrive in pieces.** A `tool_use` block opens with its id and name, then
//! its input turns up as a series of `input_json_delta` fragments that are only valid JSON
//! once concatenated. The contract says a
//! [`CompletionChunk::ToolCall`](aik_api::model::CompletionChunk::ToolCall) is a *complete*
//! call, so fragments are accumulated per block index and emitted when the block closes. A
//! fragment sequence that does not parse is an error, never an empty argument object: a tool
//! invoked with `{}` because its arguments were lost is a tool invoked with the wrong
//! arguments.
//!
//! **The peer is remote.** Both buffers are bounded, so a server that never emits a newline
//! or never closes a block fails with an error instead of consuming the process's memory.

use std::collections::HashMap;

use aik_api::model::{CompletionChunk, ContentPart, FinishReason, Usage};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::{Error, Result};
use bytes::Bytes;
use futures::StreamExt;
use futures_core::Stream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::deadline::Deadline;
use crate::http::map_reqwest_error;
use crate::protocol::{map_stop_reason, stream_error};

/// The most a single event may occupy before the stream is abandoned.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// The most one tool call's arguments may occupy while being reassembled.
const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// A `tool_use` block being assembled.
#[derive(Debug)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// What the events seen so far add up to.
#[derive(Debug, Default)]
struct StreamState {
    pending: HashMap<u64, PendingCall>,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
}

impl StreamState {
    fn usage(&self) -> Option<Usage> {
        match (self.input_tokens, self.output_tokens) {
            (0, 0) => None,
            (input_tokens, output_tokens) => Some(Usage {
                input_tokens,
                output_tokens,
            }),
        }
    }

    fn finish_reason(&self, saw_tool_call: bool) -> FinishReason {
        match saw_tool_call {
            true => FinishReason::ToolCalls,
            false => map_stop_reason(self.stop_reason.as_deref()),
        }
    }
}

/// What one event means to the caller.
enum Step {
    /// Chunks to yield.
    Emit(Vec<CompletionChunk>),
    /// The stream is over.
    End,
    /// Nothing to report — a ping, an opening block, a delta that was folded into state.
    Continue,
}

/// Parses a byte stream of SSE events into completion chunks.
///
/// The deadline applies to the whole stream, checked once per await point, rather than
/// resetting on every event: a model producing tokens steadily, however slowly, must not be
/// mistaken for a stalled one.
pub(crate) fn sse_chunks(
    byte_stream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancellation: CancellationToken,
    deadline: Deadline,
) -> impl Stream<Item = Result<CompletionChunk>> + Send + 'static {
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut byte_stream = Box::pin(byte_stream);
        let mut state = StreamState::default();
        let mut saw_tool_call = false;

        'outer: loop {
            while let Some(position) = buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=position).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }

                let Some(data) = event_data(&line) else { continue };
                let event: Value = match serde_json::from_slice(data) {
                    Ok(event) => event,
                    Err(error) => {
                        yield Err(Error::wrap("decoding an Anthropic stream event", error));
                        break 'outer;
                    }
                };

                match step(&event, &mut state) {
                    Ok(Step::Emit(chunks)) => {
                        for chunk in chunks {
                            saw_tool_call |= matches!(chunk, CompletionChunk::ToolCall(_));
                            yield Ok(chunk);
                        }
                    }
                    Ok(Step::End) => {
                        yield Ok(CompletionChunk::Done {
                            finish_reason: state.finish_reason(saw_tool_call),
                            usage: state.usage(),
                        });
                        break 'outer;
                    }
                    Ok(Step::Continue) => {}
                    Err(error) => {
                        yield Err(error);
                        break 'outer;
                    }
                }
            }

            if buffer.len() > MAX_EVENT_BYTES {
                yield Err(Error::other(
                    "the Anthropic stream sent an event larger than this provider will buffer",
                ));
                break 'outer;
            }

            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    yield Err(Error::Cancelled);
                    break 'outer;
                }
                () = tokio::time::sleep_until(deadline.instant()) => {
                    yield Err(Error::Timeout(deadline.budget()));
                    break 'outer;
                }
                next = byte_stream.next() => {
                    match next {
                        Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                        Some(Err(error)) => {
                            yield Err(map_reqwest_error(
                                "reading the Anthropic response stream",
                                error,
                            ));
                            break 'outer;
                        }
                        // The connection closed without `message_stop`. Nothing is
                        // salvageable — a half-delivered turn is not a turn — so the stream
                        // ends without a `Done`, which is what any interrupted HTTP stream
                        // looks like to its reader.
                        None => break 'outer,
                    }
                }
            }
        }
    }
}

/// Extracts the payload of a `data:` line, ignoring everything else SSE allows.
///
/// The `event:` line is ignored deliberately: every event this API sends carries the same
/// name in its JSON `type` field, and trusting one source rather than correlating two is one
/// fewer way for a malformed stream to be interpreted two ways.
fn event_data(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    Some(match rest.first() {
        Some(b' ') => &rest[1..],
        _ => rest,
    })
}

/// Folds one event into the running state, returning what the caller should emit.
fn step(event: &Value, state: &mut StreamState) -> Result<Step> {
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            if let Some(usage) = event.pointer("/message/usage") {
                state.input_tokens = field(usage, "input_tokens");
                state.output_tokens = field(usage, "output_tokens");
            }
            Ok(Step::Continue)
        }

        Some("content_block_start") => {
            let index = index_of(event)?;
            let block = event.get("content_block").unwrap_or(&Value::Null);
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                        Error::other("a streamed tool call arrived without an id")
                    })?;
                    let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                        Error::other("a streamed tool call arrived without a name")
                    })?;
                    state.pending.insert(
                        index,
                        PendingCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments: String::new(),
                        },
                    );
                    Ok(Step::Continue)
                }
                Some("text") => match block.get("text").and_then(Value::as_str) {
                    Some(text) if !text.is_empty() => Ok(Step::Emit(vec![CompletionChunk::Delta(
                        ContentPart::text(text),
                    )])),
                    _ => Ok(Step::Continue),
                },
                // A block type this crate does not model — `thinking`, a server tool's own.
                // Nothing is emitted for it, and its deltas are ignored below.
                _ => Ok(Step::Continue),
            }
        }

        Some("content_block_delta") => {
            let index = index_of(event)?;
            let delta = event.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => match delta.get("text").and_then(Value::as_str) {
                    Some(text) if !text.is_empty() => Ok(Step::Emit(vec![CompletionChunk::Delta(
                        ContentPart::text(text),
                    )])),
                    _ => Ok(Step::Continue),
                },
                Some("input_json_delta") => {
                    let fragment = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match state.pending.get_mut(&index) {
                        Some(call) => {
                            if call.arguments.len() + fragment.len() > MAX_TOOL_INPUT_BYTES {
                                return Err(Error::other(
                                    "a streamed tool call's arguments exceeded the size this \
                                     provider will buffer",
                                ));
                            }
                            call.arguments.push_str(fragment);
                            Ok(Step::Continue)
                        }
                        // Arguments for a block that never opened. Silently discarding them
                        // would produce a call with the wrong arguments if the block turns
                        // up later, so the stream stops here.
                        None => Err(Error::other(
                            "the Anthropic stream sent tool arguments for a block that was \
                             never opened",
                        )),
                    }
                }
                _ => Ok(Step::Continue),
            }
        }

        Some("content_block_stop") => {
            let index = index_of(event)?;
            match state.pending.remove(&index) {
                Some(call) => Ok(Step::Emit(vec![CompletionChunk::ToolCall(complete(call)?)])),
                None => Ok(Step::Continue),
            }
        }

        Some("message_delta") => {
            if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(reason.to_owned());
            }
            if let Some(usage) = event.get("usage") {
                let output = field(usage, "output_tokens");
                if output > 0 {
                    state.output_tokens = output;
                }
                let input = field(usage, "input_tokens");
                if input > 0 {
                    state.input_tokens = input;
                }
            }
            Ok(Step::Continue)
        }

        Some("message_stop") => Ok(Step::End),

        Some("error") => Err(stream_error(event.get("error").unwrap_or(event))),

        // `ping`, and anything added to the protocol after this was written.
        _ => Ok(Step::Continue),
    }
}

/// Turns an assembled `tool_use` block into a call.
fn complete(call: PendingCall) -> Result<ToolCall> {
    let arguments = match call.arguments.trim().is_empty() {
        // A tool taking no arguments streams no fragments at all.
        true => Value::Object(serde_json::Map::new()),
        false => serde_json::from_str(&call.arguments).map_err(|error| {
            Error::wrap(
                format!(
                    "the Anthropic stream sent arguments for `{}` that are not valid JSON",
                    call.name
                ),
                error,
            )
        })?,
    };

    Ok(ToolCall {
        call_id: call.id,
        name: ToolName::new(call.name),
        arguments,
    })
}

fn index_of(event: &Value) -> Result<u64> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::other("an Anthropic stream event arrived without a block index"))
}

fn field(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::{ManualClock, SharedClock, Timestamp};
    use futures::stream;
    use std::sync::Arc;
    use std::time::Duration;

    fn far_future() -> Deadline {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        Deadline::compute(
            &clock,
            Duration::from_secs(60),
            &aik_api::execution::ExecutionContext::new(),
        )
    }

    fn event(name: &str, data: &str) -> reqwest::Result<Bytes> {
        Ok(Bytes::from(format!("event: {name}\ndata: {data}\n\n")))
    }

    async fn collect(events: Vec<reqwest::Result<Bytes>>) -> Vec<Result<CompletionChunk>> {
        sse_chunks(stream::iter(events), CancellationToken::new(), far_future())
            .collect()
            .await
    }

    #[tokio::test]
    async fn text_deltas_are_yielded_in_order_then_done() {
        let chunks = collect(vec![
            event("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":9,"output_tokens":1}}}"#),
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("hel"))
        );
        assert_eq!(
            chunks[2].as_ref().unwrap(),
            &CompletionChunk::Done {
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 9,
                    output_tokens: 4
                }),
            }
        );
    }

    #[tokio::test]
    async fn a_tool_call_is_emitted_whole_once_its_arguments_close() {
        let chunks = collect(vec![
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"filesystem.read","input":{}}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        // Nothing is emitted until the block closes.
        assert_eq!(chunks.len(), 2);
        match chunks[0].as_ref().unwrap() {
            CompletionChunk::ToolCall(call) => {
                assert_eq!(call.call_id, "toolu_1");
                assert_eq!(call.name.as_str(), "filesystem.read");
                assert_eq!(call.arguments, serde_json::json!({ "path": "a.txt" }));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert!(matches!(
            chunks[1].as_ref().unwrap(),
            CompletionChunk::Done {
                finish_reason: FinishReason::ToolCalls,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn parallel_tool_calls_are_kept_apart_by_index() {
        let chunks = collect(vec![
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"one"}}"#),
            event("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"b","name":"two"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"n\":2}"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"n\":1}"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        let calls: Vec<&ToolCall> = chunks
            .iter()
            .filter_map(|chunk| match chunk.as_ref().unwrap() {
                CompletionChunk::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, "a");
        assert_eq!(calls[0].arguments, serde_json::json!({ "n": 1 }));
        assert_eq!(calls[1].call_id, "b");
        assert_eq!(calls[1].arguments, serde_json::json!({ "n": 2 }));
    }

    #[tokio::test]
    async fn a_tool_call_with_no_arguments_gets_an_empty_object() {
        let chunks = collect(vec![
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"now"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        match chunks[0].as_ref().unwrap() {
            CompletionChunk::ToolCall(call) => {
                assert_eq!(call.arguments, serde_json::json!({}));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn truncated_tool_arguments_are_an_error_not_an_empty_call() {
        let chunks = collect(vec![
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"rm"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/et"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        assert!(chunks[0].is_err(), "{:?}", chunks[0]);
        assert_eq!(chunks.len(), 1, "the stream stops at the first error");
    }

    #[tokio::test]
    async fn arguments_for_a_block_that_never_opened_are_refused() {
        let chunks = collect(vec![event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        )])
        .await;

        assert!(chunks[0].is_err());
    }

    #[tokio::test]
    async fn an_error_event_ends_the_stream_with_its_message() {
        let chunks = collect(vec![
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#),
            event("error", r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
        ])
        .await;

        assert_eq!(chunks.len(), 2);
        let error = chunks[1].as_ref().unwrap_err();
        let source = std::error::Error::source(error).unwrap().to_string();
        assert!(source.contains("overloaded_error"), "{source}");
    }

    #[tokio::test]
    async fn events_split_across_byte_boundaries_are_reassembled() {
        let chunks = collect(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"content_block_delta\",\"index\":0,",
            )),
            Ok(Bytes::from_static(
                b"\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            )),
            Ok(Bytes::from_static(b"data: {\"type\":\"message_stop\"}\n\n")),
        ])
        .await;

        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("hi"))
        );
    }

    #[tokio::test]
    async fn unknown_events_and_pings_are_ignored() {
        let chunks = collect(vec![
            event("ping", r#"{"type":"ping"}"#),
            event("something_new", r#"{"type":"something_new","payload":1}"#),
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ])
        .await;

        assert_eq!(chunks.len(), 1);
        assert!(matches!(
            chunks[0].as_ref().unwrap(),
            CompletionChunk::Done { .. }
        ));
    }

    #[tokio::test]
    async fn a_connection_that_drops_mid_turn_ends_without_a_done() {
        let chunks = collect(vec![event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
        )])
        .await;

        assert_eq!(chunks.len(), 1);
        assert!(matches!(
            chunks[0].as_ref().unwrap(),
            CompletionChunk::Delta(_)
        ));
    }

    #[tokio::test]
    async fn a_malformed_event_stops_the_stream() {
        let chunks = collect(vec![event("message_delta", "{not json")]).await;
        assert!(chunks[0].is_err());
    }

    #[tokio::test]
    async fn cancellation_is_reported() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let chunks: Vec<Result<CompletionChunk>> =
            sse_chunks(stream::pending(), cancellation, far_future())
                .collect()
                .await;

        assert!(
            matches!(chunks[0], Err(Error::Cancelled)),
            "{:?}",
            chunks[0]
        );
    }
}
