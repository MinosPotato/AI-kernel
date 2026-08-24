//! Length-prefixed JSON frames.
//!
//! A stream socket is a stream of bytes, so the first thing any protocol over one has to
//! decide is where a message ends. This one says so up front: four bytes of big-endian
//! length, then that many bytes of JSON.
//!
//! # The bound is the point
//!
//! [`MAX_FRAME_BYTES`] is checked *before* anything is allocated, and a frame larger than it
//! ends the connection rather than being skipped. Both halves matter:
//!
//! * Allocating first would mean a peer that writes `0xFFFFFFFF` and then nothing at all costs
//!   the host four gigabytes and a stalled task. Reading the length is not permission to
//!   believe it.
//! * Skipping an oversized frame and carrying on would leave the two sides disagreeing about
//!   where the next message starts, which is worse than disconnecting: the remaining bytes of
//!   the discarded frame would be parsed as new messages.
//!
//! A zero-length frame is refused for the same reason — it carries no JSON, so it can only be
//! a peer that is confused or probing, and there is no useful way to carry on from one.
//!
//! # Why JSON
//!
//! Everything on the wire is already a `serde` type that the kernel defines and the audit
//! trail stores: [`AgentUpdate`](aik_api::agent::AgentUpdate),
//! [`AuditRecord`](aik_api::audit::AuditRecord),
//! [`PendingApproval`](aik_approval::PendingApproval). A binary encoding would buy throughput
//! this protocol does not need — the peers are on one machine and the payloads are a
//! conversation — at the cost of a second representation of every one of those types.

use aik_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame either side will send or accept, in bytes.
///
/// One mebibyte: far more than a prompt, a batch of audit records or a window of agent
/// updates needs, and small enough that a peer cannot make the host allocate meaningfully by
/// asking. A request that genuinely does not fit is a request that should have been paged.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// The number of bytes the length prefix occupies.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Writes one value as a frame.
///
/// The frame is refused rather than truncated if it does not fit, so the two sides can never
/// disagree about a message boundary because of something this end produced.
pub async fn write<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let body = serde_json::to_vec(value).map_err(Error::Serialization)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(Error::InvalidArgument(format!(
            "a {}-byte message is larger than the {MAX_FRAME_BYTES}-byte frame limit",
            body.len(),
        )));
    }
    let length = u32::try_from(body.len()).expect("a length already bounded by MAX_FRAME_BYTES");

    // One write of prefix and body together rather than two: a frame half-written by a task
    // that is then cancelled would desynchronise the stream, and this leaves a much smaller
    // window for that than a prefix write awaited separately from a body write.
    let mut framed = Vec::with_capacity(LENGTH_PREFIX_BYTES + body.len());
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&body);

    writer
        .write_all(&framed)
        .await
        .map_err(|error| Error::wrap("writing a frame", error))?;
    writer
        .flush()
        .await
        .map_err(|error| Error::wrap("flushing a frame", error))
}

/// Reads one frame, or `None` if the peer closed the connection between frames.
///
/// A connection that ends *within* a frame is an error rather than `None`: the difference
/// between "the peer is finished" and "the peer vanished mid-message" is worth keeping, and
/// only the first is a clean close.
pub async fn read<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    match reader.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(Error::wrap("reading a frame length", error)),
    }

    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(Error::InvalidArgument(
            "a frame must carry a message; a zero-length one carries nothing".to_owned(),
        ));
    }
    // Checked before the allocation, not after it: a peer that says four gigabytes must not
    // be able to make this end try.
    if length > MAX_FRAME_BYTES {
        return Err(Error::InvalidArgument(format!(
            "a peer announced a {length}-byte frame, over the {MAX_FRAME_BYTES}-byte limit",
        )));
    }

    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| Error::wrap("reading a frame body", error))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(Error::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Message {
        text: String,
    }

    fn message(text: &str) -> Message {
        Message {
            text: text.to_owned(),
        }
    }

    #[tokio::test]
    async fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        write(&mut buffer, &message("one")).await.expect("written");
        write(&mut buffer, &message("two")).await.expect("written");

        let mut reader = buffer.as_slice();
        assert_eq!(
            read::<_, Message>(&mut reader).await.expect("read"),
            Some(message("one")),
        );
        assert_eq!(
            read::<_, Message>(&mut reader).await.expect("read"),
            Some(message("two")),
        );
        assert_eq!(read::<_, Message>(&mut reader).await.expect("read"), None);
    }

    #[tokio::test]
    async fn an_oversized_announcement_is_refused_without_allocating() {
        // Four gigabytes announced, four bytes delivered. The point of the test is that this
        // returns rather than trying.
        let mut framed = u32::MAX.to_be_bytes().to_vec();
        framed.extend_from_slice(b"oops");

        let error = read::<_, Message>(&mut framed.as_slice())
            .await
            .expect_err("an impossible frame must be refused");
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[tokio::test]
    async fn a_zero_length_frame_is_refused() {
        let framed = 0u32.to_be_bytes().to_vec();
        let error = read::<_, Message>(&mut framed.as_slice())
            .await
            .expect_err("an empty frame carries no message");
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_error_rather_than_a_clean_close() {
        let mut framed = 64u32.to_be_bytes().to_vec();
        framed.extend_from_slice(b"{\"text\":\"short\"}");

        let error = read::<_, Message>(&mut framed.as_slice())
            .await
            .expect_err("a peer that vanished mid-frame is not a peer that finished");
        assert!(error.to_string().contains("frame body"), "{error}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_expected_shape_is_a_serialization_error() {
        let mut buffer = Vec::new();
        write(&mut buffer, &serde_json::json!({ "unexpected": 1 }))
            .await
            .expect("written");

        let error = read::<_, Message>(&mut buffer.as_slice())
            .await
            .expect_err("a well-framed message of the wrong shape is still refused");
        assert!(matches!(error, Error::Serialization(_)), "{error}");
    }

    #[test]
    fn the_frame_limit_is_part_of_the_protocol() {
        // Both ends have to agree on it: a host that accepted more than a client would send
        // is merely generous, but a host that accepted *less* would refuse messages a
        // conforming client is entitled to send, and the refusal would look like corruption.
        // So the value is asserted rather than merely derived from itself.
        assert_eq!(MAX_FRAME_BYTES, 1024 * 1024);
        assert_eq!(LENGTH_PREFIX_BYTES, 4);
    }

    #[tokio::test]
    async fn a_frame_of_exactly_the_limit_is_within_it() {
        // The boundary in both directions. Off by one here means either a message that can be
        // written and not read — which desynchronises the stream — or a limit that is quietly
        // one byte smaller than it says.
        let overhead = serde_json::to_vec(&message("")).expect("encoded").len();
        let exact = message(&"x".repeat(MAX_FRAME_BYTES - overhead));

        let mut buffer = Vec::new();
        write(&mut buffer, &exact)
            .await
            .expect("exactly the limit fits");
        assert_eq!(buffer.len(), LENGTH_PREFIX_BYTES + MAX_FRAME_BYTES);

        let mut reader = buffer.as_slice();
        assert_eq!(
            read::<_, Message>(&mut reader).await.expect("read"),
            Some(exact),
        );
    }

    /// A reader that fails with something other than end of input.
    struct Broken;

    impl tokio::io::AsyncRead for Broken {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            )))
        }
    }

    #[tokio::test]
    async fn a_stream_that_breaks_is_not_a_peer_that_finished() {
        // Only end of input is a clean close. Reading any other failure as one would turn a
        // connection that died into a connection that said goodbye, and the caller would tidy
        // up as if nothing had gone wrong.
        let error = read::<_, Message>(&mut Broken)
            .await
            .expect_err("a broken stream is not a clean close");
        assert!(error.to_string().contains("frame length"), "{error}");
    }

    #[tokio::test]
    async fn writing_more_than_the_limit_is_refused_rather_than_truncated() {
        let huge = message(&"x".repeat(MAX_FRAME_BYTES + 1));
        let mut buffer = Vec::new();
        let error = write(&mut buffer, &huge)
            .await
            .expect_err("a message over the limit must not be sent at all");
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
        assert!(
            buffer.is_empty(),
            "nothing may reach the stream, or the peer loses the frame boundary",
        );
    }
}
