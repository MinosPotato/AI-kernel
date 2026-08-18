//! Turns Ollama's newline-delimited JSON stream into a stream of [`CompletionChunk`]s.

use aik_api::model::CompletionChunk;
use aik_core::{Error, Result};
use bytes::Bytes;
use futures::StreamExt;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::deadline::Deadline;
use crate::http::map_reqwest_error;
use crate::protocol::parse_line;

/// Parses a byte stream of NDJSON lines into completion chunks.
///
/// The deadline applies to the *whole* stream, checked once per await point, rather than
/// resetting on every chunk — a model producing tokens steadily, however slowly, must not
/// be mistaken for a stalled one. Cancellation is checked the same way.
pub(crate) fn ndjson_chunks(
    byte_stream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    cancellation: CancellationToken,
    deadline: Deadline,
) -> impl Stream<Item = Result<CompletionChunk>> + Send + 'static {
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut byte_stream = Box::pin(byte_stream);

        'outer: loop {
            while let Some(pos) = buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=pos).collect();
                line.pop(); // drop the newline itself
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match parse_line(&line) {
                    Ok(Some(chunk)) => {
                        let done = matches!(chunk, CompletionChunk::Done { .. });
                        yield Ok(chunk);
                        if done {
                            break 'outer;
                        }
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        yield Err(error);
                        break 'outer;
                    }
                }
            }

            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    yield Err(Error::Cancelled);
                    break 'outer;
                }
                () = tokio::time::sleep_until(deadline.instant()) => {
                    yield Err(Error::Timeout(std::time::Duration::default()));
                    break 'outer;
                }
                next = byte_stream.next() => {
                    match next {
                        Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                        Some(Err(error)) => {
                            yield Err(map_reqwest_error("reading the Ollama response stream", error));
                            break 'outer;
                        }
                        None => {
                            // The connection closed. A trailing unterminated line still
                            // carrying content is salvaged; otherwise there is nothing left
                            // to report and the stream simply ends without an explicit
                            // `done`, matching what a client that lost the connection would
                            // see for any HTTP stream.
                            if !buffer.iter().all(u8::is_ascii_whitespace) {
                                if let Ok(Some(chunk)) = parse_line(&buffer) {
                                    yield Ok(chunk);
                                }
                            }
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::model::{ContentPart, FinishReason};
    use aik_core::clock::{ManualClock, SharedClock, Timestamp};
    use futures::stream;
    use std::sync::Arc;
    use std::time::Duration;

    fn far_future_deadline() -> Deadline {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        Deadline::compute(
            &clock,
            Duration::from_secs(60),
            &aik_api::execution::ExecutionContext::new(),
        )
    }

    fn line(json: &str) -> reqwest::Result<Bytes> {
        Ok(Bytes::from(format!("{json}\n")))
    }

    #[tokio::test]
    async fn deltas_are_yielded_in_order_then_done() {
        let byte_stream = stream::iter(vec![
            line(r#"{"message":{"role":"assistant","content":"hel"},"done":false}"#),
            line(r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#),
            line(r#"{"done":true,"done_reason":"stop"}"#),
        ]);

        let chunks: Vec<Result<CompletionChunk>> =
            ndjson_chunks(byte_stream, CancellationToken::new(), far_future_deadline())
                .collect()
                .await;

        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("hel"))
        );
        assert_eq!(
            chunks[1].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("lo"))
        );
        assert_eq!(
            chunks[2].as_ref().unwrap(),
            &CompletionChunk::Done {
                finish_reason: FinishReason::Stop,
                usage: None
            }
        );
    }

    #[tokio::test]
    async fn a_line_split_across_two_byte_chunks_is_reassembled() {
        let byte_stream = stream::iter(vec![
            Ok(Bytes::from_static(br#"{"message":{"role":"ass"#)),
            Ok(Bytes::from_static(
                b"istant\",\"content\":\"hi\"},\"done\":false}\n",
            )),
            line(r#"{"done":true}"#),
        ]);

        let chunks: Vec<Result<CompletionChunk>> =
            ndjson_chunks(byte_stream, CancellationToken::new(), far_future_deadline())
                .collect()
                .await;

        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("hi"))
        );
    }

    #[tokio::test]
    async fn an_error_line_ends_the_stream() {
        let byte_stream = stream::iter(vec![line(r#"{"error":"boom"}"#)]);

        let chunks: Vec<Result<CompletionChunk>> =
            ndjson_chunks(byte_stream, CancellationToken::new(), far_future_deadline())
                .collect()
                .await;

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_err());
    }

    #[tokio::test]
    async fn cancelling_mid_stream_stops_it_with_a_cancelled_error() {
        let cancellation = CancellationToken::new();
        let cancel_for_stream = cancellation.clone();

        // A stream that yields one line and then hangs forever, simulating a model that is
        // still generating when the caller gives up.
        let byte_stream = stream::once(async move {
            line(r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#)
        })
        .chain(stream::pending());

        let mut chunks = Box::pin(ndjson_chunks(
            byte_stream,
            cancel_for_stream,
            far_future_deadline(),
        ));

        let first = chunks.next().await.unwrap();
        assert_eq!(
            first.unwrap(),
            CompletionChunk::Delta(ContentPart::text("hi"))
        );

        cancellation.cancel();
        let second = chunks.next().await.unwrap();
        assert!(matches!(second, Err(Error::Cancelled)), "{second:?}");
    }

    #[tokio::test]
    async fn a_passed_deadline_stops_the_stream_with_a_timeout() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let deadline = Deadline::compute(
            &clock,
            Duration::from_millis(5),
            &aik_api::execution::ExecutionContext::new(),
        );

        let byte_stream = stream::pending();
        let result: Vec<Result<CompletionChunk>> =
            ndjson_chunks(byte_stream, CancellationToken::new(), deadline)
                .collect()
                .await;

        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(Error::Timeout(_))), "{result:?}");
    }

    #[tokio::test]
    async fn a_trailing_unterminated_line_is_salvaged() {
        let byte_stream = stream::iter(vec![Ok(Bytes::from_static(
            br#"{"message":{"role":"assistant","content":"tail"},"done":false}"#,
        ))]);

        let chunks: Vec<Result<CompletionChunk>> =
            ndjson_chunks(byte_stream, CancellationToken::new(), far_future_deadline())
                .collect()
                .await;

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].as_ref().unwrap(),
            &CompletionChunk::Delta(ContentPart::text("tail"))
        );
    }
}
