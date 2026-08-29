//! Turns the chat-completions server-sent event stream into [`CompletionChunk`]s.
//!
//! Three things make this more than a line parser.
//!
//! **Tool calls arrive in pieces, and nothing announces that one is finished.** A call turns
//! up as a series of deltas sharing an `index`: the first usually carries the id and the
//! name, and the rest carry fragments of an arguments string that is only valid JSON once
//! concatenated. Unlike the Anthropic stream there is no per-block stop event, so calls are
//! held until the choice reports a `finish_reason` — or, failing that, until the stream ends
//! — and emitted then. The contract says a
//! [`CompletionChunk::ToolCall`](aik_api::model::CompletionChunk::ToolCall) is a *complete*
//! call, and a fragment sequence that does not parse is an error, never an empty argument
//! object: a tool invoked with `{}` because its arguments were lost is a tool invoked with
//! the wrong arguments.
//!
//! **Usage arrives after the answer does.** The final `finish_reason` is not the last event;
//! a further chunk with no choices carries the token counts, which is why
//! [`CompletionChunk::Done`](aik_api::model::CompletionChunk::Done) is emitted at `[DONE]`
//! rather than at the finish reason. A stream that ends *without* `[DONE]` but *with* a
//! finish reason is still complete — several servers in this family simply omit the
//! sentinel — while one that ends before any finish reason is a cut turn, and ends without a
//! `Done` just as any interrupted HTTP stream does.
//!
//! **The peer is remote.** Both buffers are bounded, so a server that never emits a newline
//! or never closes a call fails with an error instead of consuming the process's memory.

use std::collections::BTreeMap;

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
use crate::protocol::{map_finish_reason, parse_arguments, stream_error};

/// The most a single event may occupy before the stream is abandoned.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// The most one tool call's arguments may occupy while being reassembled.
const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// The sentinel that ends a stream in this dialect.
const DONE: &[u8] = b"[DONE]";

/// A tool call being assembled out of deltas that share an index.
#[derive(Debug, Default)]
struct PendingCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// What the events seen so far add up to.
///
/// The pending calls are a [`BTreeMap`] rather than a hash map so that flushing them is in
/// index order, which is the order the model asked for them in.
#[derive(Debug, Default)]
struct StreamState {
    pending: BTreeMap<u64, PendingCall>,
    input_tokens: u64,
    output_tokens: u64,
    finish_reason: Option<String>,
    saw_tool_call: bool,
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

    fn done(&self) -> CompletionChunk {
        CompletionChunk::Done {
            finish_reason: match self.saw_tool_call {
                true => FinishReason::ToolCalls,
                false => map_finish_reason(self.finish_reason.as_deref()),
            },
            usage: self.usage(),
        }
    }

    /// Turns every call assembled so far into a chunk, emptying the pending set.
    fn flush(&mut self) -> Result<Vec<CompletionChunk>> {
        let pending = std::mem::take(&mut self.pending);
        let mut chunks = Vec::with_capacity(pending.len());
        for (index, call) in pending {
            chunks.push(CompletionChunk::ToolCall(complete(index, call)?));
            self.saw_tool_call = true;
        }
        Ok(chunks)
    }
}

/// What one event means to the caller.
enum Step {
    /// Chunks to yield.
    Emit(Vec<CompletionChunk>),
    /// The stream is over.
    End,
    /// Nothing to report — a keep-alive, or a delta that was folded into state.
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

        'outer: loop {
            while let Some(position) = buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=position).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }

                let Some(data) = event_data(&line) else { continue };

                let step = match data == DONE {
                    true => Ok(Step::End),
                    false => match serde_json::from_slice::<Value>(data) {
                        Ok(event) => step(&event, &mut state),
                        Err(error) => {
                            yield Err(Error::wrap("decoding an OpenAI stream event", error));
                            break 'outer;
                        }
                    },
                };

                match step {
                    Ok(Step::Emit(chunks)) => {
                        for chunk in chunks {
                            yield Ok(chunk);
                        }
                    }
                    Ok(Step::End) => {
                        // A server that sent `[DONE]` without ever reporting a finish reason
                        // may still have left a call assembled.
                        match state.flush() {
                            Ok(chunks) => {
                                for chunk in chunks {
                                    yield Ok(chunk);
                                }
                            }
                            Err(error) => {
                                yield Err(error);
                                break 'outer;
                            }
                        }
                        yield Ok(state.done());
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
                    "the OpenAI stream sent an event larger than this provider will buffer",
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
                                "reading the OpenAI response stream",
                                error,
                            ));
                            break 'outer;
                        }
                        None => {
                            // No `[DONE]`. A turn that got as far as a finish reason is
                            // complete — several servers in this family omit the sentinel —
                            // and anything earlier is a connection that was cut mid-answer,
                            // which ends without a `Done` like any interrupted stream.
                            if state.finish_reason.is_some() {
                                yield Ok(state.done());
                            }
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
}

/// Extracts the payload of a `data:` line, ignoring everything else SSE allows.
fn event_data(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    Some(match rest.first() {
        Some(b' ') => &rest[1..],
        _ => rest,
    })
}

/// Folds one event into the running state, returning what the caller should emit.
fn step(event: &Value, state: &mut StreamState) -> Result<Step> {
    // Some servers report a mid-stream failure as an ordinary data frame rather than by
    // cutting the connection.
    if let Some(error) = event.get("error").filter(|value| !value.is_null()) {
        return Err(stream_error(error));
    }

    if let Some(usage) = event.get("usage").filter(|value| !value.is_null()) {
        let input = field(usage, "prompt_tokens");
        if input > 0 {
            state.input_tokens = input;
        }
        let output = field(usage, "completion_tokens");
        if output > 0 {
            state.output_tokens = output;
        }
    }

    // Only the first choice is read. Asking for more than one is refused when the request is
    // built, so a second choice here is a server doing something nobody asked for, and
    // interleaving two answers into one message would be worse than ignoring it.
    let Some(choice) = event
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(Step::Continue);
    };

    let mut emitted = Vec::new();

    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        emitted.push(CompletionChunk::Delta(ContentPart::text(text)));
    }

    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            accumulate(call, state)?;
        }
    }

    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = Some(reason.to_owned());
        // Nothing else announces that a call is finished, so this is the point at which one
        // can be handed over.
        emitted.extend(state.flush()?);
    }

    match emitted.is_empty() {
        true => Ok(Step::Continue),
        false => Ok(Step::Emit(emitted)),
    }
}

/// Folds one tool-call delta into the call it belongs to.
fn accumulate(call: &Value, state: &mut StreamState) -> Result<()> {
    // The index is what ties fragments together. Without it there is no way to tell a
    // continuation of one call from the start of another.
    let index = call.get("index").and_then(Value::as_u64).ok_or_else(|| {
        Error::other("an OpenAI stream sent a tool call fragment without an index")
    })?;

    let pending = state.pending.entry(index).or_default();

    // The id and the name normally arrive on the first fragment, but a server is free to
    // send them later. Neither is ever overwritten once known: a second, different id for
    // the same index would leave the result matched to the wrong call.
    if let Some(id) = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        pending.id.get_or_insert_with(|| id.to_owned());
    }
    if let Some(name) = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        pending.name.get_or_insert_with(|| name.to_owned());
    }

    if let Some(fragment) = call.pointer("/function/arguments").and_then(Value::as_str) {
        if pending.arguments.len() + fragment.len() > MAX_TOOL_INPUT_BYTES {
            return Err(Error::other(
                "a streamed tool call's arguments exceeded the size this provider will buffer",
            ));
        }
        pending.arguments.push_str(fragment);
    }

    Ok(())
}

/// Turns an assembled call into a [`ToolCall`], or says what was missing.
fn complete(index: u64, call: PendingCall) -> Result<ToolCall> {
    let id = call.id.ok_or_else(|| {
        Error::other(format!(
            "the OpenAI stream never sent an id for the tool call at index {index}, so its \
             result could not be matched to it"
        ))
    })?;
    let name = call.name.ok_or_else(|| {
        Error::other(format!(
            "the OpenAI stream never sent a name for the tool call at index {index}"
        ))
    })?;
    let arguments = parse_arguments(&call.arguments, &name)?;

    Ok(ToolCall {
        call_id: id,
        name: ToolName::new(&name),
        arguments,
    })
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

    fn frame(data: &str) -> reqwest::Result<Bytes> {
        Ok(Bytes::from(format!("data: {data}\n\n")))
    }

    fn done() -> reqwest::Result<Bytes> {
        Ok(Bytes::from_static(b"data: [DONE]\n\n"))
    }

    async fn collect(events: Vec<reqwest::Result<Bytes>>) -> Vec<Result<CompletionChunk>> {
        sse_chunks(stream::iter(events), CancellationToken::new(), far_future())
            .collect()
            .await
    }

    #[tokio::test]
    async fn text_deltas_are_yielded_in_order_then_done() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"content":"hel"}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            frame(r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#),
            done(),
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
    async fn a_tool_call_is_emitted_whole_once_the_choice_finishes() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"filesystem.read","arguments":""}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
        ])
        .await;

        // Nothing is emitted until the choice reports it is finished.
        assert_eq!(chunks.len(), 2);
        match chunks[0].as_ref().unwrap() {
            CompletionChunk::ToolCall(call) => {
                assert_eq!(call.call_id, "call_1");
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
    async fn parallel_tool_calls_are_kept_apart_by_index_and_emitted_in_order() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"two","arguments":"{\"n\":2}"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"one","arguments":"{\"n\":1}"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
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
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"now","arguments":""}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
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
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"rm","arguments":"{\"path\":\"/et"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
        ])
        .await;

        assert!(chunks[0].is_err(), "{:?}", chunks[0]);
        assert_eq!(chunks.len(), 1, "the stream stops at the first error");
    }

    #[tokio::test]
    async fn a_tool_call_fragment_without_an_index_is_refused() {
        let chunks = collect(vec![frame(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"a","function":{"name":"x"}}]}}]}"#,
        )])
        .await;
        assert!(chunks[0].is_err());
    }

    #[tokio::test]
    async fn a_call_that_never_got_an_id_is_refused_rather_than_invented() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"x","arguments":"{}"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
        ])
        .await;
        let error = chunks[0].as_ref().unwrap_err();
        assert!(format!("{error}").contains("never sent an id"), "{error}");
    }

    #[tokio::test]
    async fn a_second_id_for_the_same_index_never_replaces_the_first() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"first","function":{"name":"x","arguments":"{}"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"second","function":{"name":"y"}}]}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            done(),
        ])
        .await;

        match chunks[0].as_ref().unwrap() {
            CompletionChunk::ToolCall(call) => {
                assert_eq!(call.call_id, "first");
                assert_eq!(call.name.as_str(), "x");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stream_that_omits_the_done_sentinel_is_still_complete() {
        // Several servers in this family never send `[DONE]`.
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
        ])
        .await;

        assert_eq!(chunks.len(), 2);
        assert!(matches!(
            chunks[1].as_ref().unwrap(),
            CompletionChunk::Done {
                finish_reason: FinishReason::Stop,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_connection_that_drops_mid_turn_ends_without_a_done() {
        let chunks = collect(vec![frame(
            r#"{"choices":[{"index":0,"delta":{"content":"half"}}]}"#,
        )])
        .await;

        assert_eq!(chunks.len(), 1);
        assert!(matches!(
            chunks[0].as_ref().unwrap(),
            CompletionChunk::Delta(_)
        ));
    }

    #[tokio::test]
    async fn a_done_sentinel_flushes_a_call_no_finish_reason_closed() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"now","arguments":"{}"}}]}}]}"#),
            done(),
        ])
        .await;

        assert!(matches!(
            chunks[0].as_ref().unwrap(),
            CompletionChunk::ToolCall(_)
        ));
        assert!(matches!(
            chunks[1].as_ref().unwrap(),
            CompletionChunk::Done {
                finish_reason: FinishReason::ToolCalls,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_error_frame_ends_the_stream_with_its_message() {
        let chunks = collect(vec![
            frame(r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#),
            frame(r#"{"error":{"type":"server_error","message":"upstream died"}}"#),
        ])
        .await;

        assert_eq!(chunks.len(), 2);
        let error = chunks[1].as_ref().unwrap_err();
        let source = std::error::Error::source(error).unwrap().to_string();
        assert!(source.contains("server_error"), "{source}");
        assert!(source.contains("upstream died"), "{source}");
    }

    #[tokio::test]
    async fn events_split_across_byte_boundaries_are_reassembled() {
        let chunks = collect(vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[{\"index\":0,")),
            Ok(Bytes::from_static(b"\"delta\":{\"content\":\"hi\"}}]}\n\n")),
            done(),
        ])
        .await;

        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("hi"))
        );
    }

    #[tokio::test]
    async fn keep_alives_and_unknown_fields_are_ignored() {
        let chunks = collect(vec![
            Ok(Bytes::from_static(b": ping\n\n")),
            frame(r#"{"choices":[{"index":0,"delta":{"reasoning_content":"hmm"}}]}"#),
            frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            done(),
        ])
        .await;

        assert_eq!(chunks.len(), 1);
        assert!(matches!(
            chunks[0].as_ref().unwrap(),
            CompletionChunk::Done { .. }
        ));
    }

    #[tokio::test]
    async fn a_malformed_event_stops_the_stream() {
        let chunks = collect(vec![frame("{not json")]).await;
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
