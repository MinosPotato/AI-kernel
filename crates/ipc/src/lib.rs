//! The authenticated local protocol between the AI kernel's host process and its clients.
//!
//! One process owns the database and the kernel over it — see
//! [`aik-daemon`](../aik_daemon/index.html) — because redb locks the file and because a
//! schedule needs something that is always there. Everything else talks to that process, over
//! a Unix socket, using what is in this crate.
//!
//! ```text
//!   aik --socket …                          aikd
//!   ┌────────────────┐                      ┌──────────────────────────────┐
//!   │ Client         │  Hello  ───────────▶ │ Listener                     │
//!   │                │ ◀─────────  Welcome  │  1. peer uid, from the kernel│
//!   │  send(Request) │  Call   ───────────▶ │  2. token, mode 0600         │
//!   │  recv()        │ ◀───────── Response  │  3. protocol version         │
//!   └────────────────┘                      └──────────────────────────────┘
//! ```
//!
//! # What this crate is, and is not
//!
//! It is a transport and a vocabulary. It holds no kernel, resolves no service, and decides
//! nothing about whether an operation may happen: a [`Request`] is a thing to *ask*, and every
//! answer to it is produced by the host running the same store, the same registry and the same
//! authorization a terminal run would.
//!
//! Three properties are structural rather than checked, and they are why the protocol types
//! live in their own crate where both sides compile against the same definitions:
//!
//! * **No request names a principal.** There is nowhere on the wire to put one. The host
//!   derives the single identity in play from the connection it authenticated.
//! * **No request names a tool, a handler or a policy.** A client asks for a conversation, a
//!   session listing, a schedule or an audit query. What runs, and whether it may, is the
//!   host's business.
//! * **No response carries a tool's arguments or output.** Everything on the wire is a type
//!   the kernel already defines and already documents the limits of.
//!
//! # Local only, deliberately
//!
//! Unix sockets, peer credentials, file modes. There is no listener on a network address and
//! no place to configure one. Remote access needs a transport identity that is not a uid and a
//! trust decision that is not a file mode; adding it later means adding both, and pretending
//! otherwise now would mean shipping a protocol whose entire authentication story is "the
//! filesystem" over a socket where there is no filesystem.

pub mod client;
pub mod credentials;
pub mod endpoint;
pub mod frame;
pub mod listener;
pub mod protocol;

pub use client::{Client, Connected, is_listening};
pub use credentials::{TOKEN_BYTES, Token, current_uid};
pub use endpoint::{Endpoint, SOCKET_ENV, verify_private};
pub use frame::MAX_FRAME_BYTES;
pub use listener::{Accepted, Authentication, Listener, Peer, authenticate};
pub use protocol::{
    Call, Hello, HostStatus, PROTOCOL_VERSION, RejectReason, Reply, Request, Response,
    ScheduleRequest, Welcome, WireError, WireErrorKind,
};
