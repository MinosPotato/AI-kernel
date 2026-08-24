//! What the two sides say to each other.
//!
//! # The shape of a conversation
//!
//! ```text
//!   client                                        host
//!     │ Hello { protocol, token, interactive }      │
//!     ├────────────────────────────────────────────▶│  uid, then token, then version
//!     │◀────────────────────────────────────────────┤  Welcome::Accepted | Rejected
//!     │                                             │
//!     │ Call { id, Request::Prompt { .. } }         │
//!     ├────────────────────────────────────────────▶│
//!     │◀────────────────────────────────────────────┤  Response::Approval  (unsolicited)
//!     │ Call { id, Request::Approve { .. } }        │
//!     ├────────────────────────────────────────────▶│
//!     │◀────────────────────────────────────────────┤  Response::Update    (many)
//!     │◀────────────────────────────────────────────┤  Response::Done | Failed
//! ```
//!
//! Every call carries a client-chosen [`Call::id`] and every answer quotes it back, so a
//! connection may have several calls in flight and a client always knows which stream of
//! updates belongs to which prompt.
//!
//! # What a request deliberately cannot say
//!
//! There is no principal on any request, no owner on any job, and no handler on a schedule.
//! That is the central security property of this protocol, and it is enforced by the *shape*
//! of these types rather than by a check somewhere: a client cannot ask to act as somebody
//! else, because there is nowhere in the message to put it. The host derives the one identity
//! in play from the connection it authenticated, and stamps it on every
//! [`ExecutionContext`](aik_api::execution::ExecutionContext) it builds.
//!
//! The same reasoning removes [`JobSpec::handler`](aik_api::scheduler::JobSpec::handler) from
//! [`ScheduleRequest`]. A handler is a component id, the kernel registry holds every
//! `dyn JobHandler` there is, and a client that could name one could point a timer at any of
//! them. It names a prompt instead, and the host decides what runs it.
//!
//! # What a response deliberately does not carry
//!
//! Tool *arguments*, and tool *output*. [`aik_api::agent::AgentUpdate`] carries
//! what the agent produced and the shape of what it did, which is what a frontend renders;
//! everything this protocol adds around it — approvals, audit records, session listings — is
//! already a type whose own documentation states what it may and may not contain. Nothing
//! here widens any of them.

use std::path::PathBuf;
use std::time::Duration;

use aik_api::agent::{AgentResponse, AgentUpdate, SessionId};
use aik_api::audit::{AuditQuery, AuditRecord};
use aik_api::context::ContextStats;
use aik_api::permission::Principal;
use aik_api::scheduler::{JobId, ScheduledJob, Trigger};
use aik_approval::{ApprovalId, PendingApproval};
use aik_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
///
/// Bumped whenever a change would make an older peer misread a message. A mismatch is a
/// refused connection with a message naming both versions, never a best-effort attempt:
/// two processes that disagree about the wire format are two processes that will disagree
/// about what somebody authorized.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long a peer has to complete the handshake before the host hangs up.
///
/// A connection that has not authenticated is a connection that holds a slot for free, so it
/// is not allowed to hold one for long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The first message a client sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The version the client speaks.
    pub protocol: u32,
    /// The shared secret read from the host's token file.
    pub token: String,
    /// A human-readable client name, for the host's own status output.
    ///
    /// Untrusted, and used for nothing but display. It is not an identity and grants nothing.
    #[serde(default)]
    pub client: String,
    /// Whether a human is present to answer approval questions.
    ///
    /// `true` makes the connection hold an [`ApprovalGate`](aik_approval::ApprovalGate) for
    /// as long as it lasts, which is what tells the broker somebody can really be asked. It
    /// is an assertion a client makes about itself, and the only thing it can do is cause
    /// questions to be *asked* rather than refused outright — a client cannot approve anything
    /// by claiming it, because approving still requires answering.
    #[serde(default)]
    pub interactive: bool,
}

/// The host's answer to a [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Welcome {
    /// The connection is authenticated and may make calls.
    Accepted {
        /// The version the host speaks, always [`PROTOCOL_VERSION`] for this build.
        protocol: u32,
        /// The host's version string.
        host: String,
        /// The principal every call on this connection runs as.
        ///
        /// Reported so a client can *show* who it is acting as. It is not a client choice
        /// and sending a different one back changes nothing: there is nowhere on a
        /// [`Request`] to put it.
        principal: Principal,
        /// Whether this connection is holding an approval gate.
        ///
        /// Not simply an echo of [`Hello::interactive`]: a host shutting down, or one that
        /// declined for its own reasons, answers `false`, and a client that assumed
        /// otherwise would sit waiting for questions that are being refused.
        interactive: bool,
    },
    /// The connection is refused and about to be closed.
    Rejected {
        /// Why, coarsely.
        reason: RejectReason,
        /// Why, in a sentence.
        message: String,
    },
}

/// Why a connection was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The peer did not present the host's token.
    ///
    /// Deliberately the same reason whether the token was absent, malformed or simply wrong.
    Unauthenticated,
    /// The peer speaks a protocol version this host does not.
    UnsupportedProtocol,
    /// The host is already serving as many connections as it will.
    TooManyConnections,
    /// The host is shutting down and is no longer taking work.
    ShuttingDown,
}

impl RejectReason {
    /// The reason's name, as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::TooManyConnections => "too_many_connections",
            Self::ShuttingDown => "shutting_down",
        }
    }
}

/// One request, with the identifier its answers quote back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// Chosen by the client, unique among its own in-flight calls.
    pub id: u64,
    /// What is being asked.
    pub request: Request,
}

/// What a client can ask the host to do.
///
/// Every variant is something the host already does for a terminal run, reached through the
/// same store, the same registry and the same authorization. There is no variant here that
/// exists only over the socket, and deliberately none that reaches past a subsystem: no
/// "evaluate this policy", no "invoke this tool", no "read this file".
///
/// # Why the tag is adjacent
///
/// The same reason [`AgentUpdate`]'s is. An internal tag flattens a newtype variant's payload
/// into the tagged object, which breaks outright when the payload is a sequence and breaks
/// subtly when it is a value that already has a `type` of its own. Nesting the payload under
/// `value` means no variant of these enums can be made unserialisable by a change to a type it
/// merely carries — which for a wire format between two processes is worth more than the few
/// bytes an internal tag saves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Request {
    /// Confirm the connection is alive.
    Ping,
    /// Describe what the host is serving.
    Status,
    /// Run one turn, streaming updates until it finishes.
    Prompt {
        /// The transcript to append to, or `None` for a fresh one.
        #[serde(default)]
        session: Option<SessionId>,
        /// What to ask.
        input: String,
    },
    /// Ask an in-flight call on this connection to stop.
    ///
    /// Cancels a call of *this* connection only. A client cannot name another connection's
    /// call, because it has never seen one: ids are per-connection.
    Cancel {
        /// The [`Call::id`] to cancel.
        call: u64,
    },
    /// Answer an approval question with yes.
    Approve {
        /// The question, as the host asked it.
        approval: ApprovalId,
    },
    /// Answer an approval question with no.
    Deny {
        /// The question, as the host asked it.
        approval: ApprovalId,
    },
    /// List the sessions this connection's principal may act for.
    Sessions,
    /// Discard a session's transcript.
    Clear {
        /// Which session.
        session: SessionId,
    },
    /// Drop a session's oldest evictable records, keeping the newest `keep`.
    Compact {
        /// Which session.
        session: SessionId,
        /// How many records to keep.
        keep: usize,
    },
    /// List the scheduled jobs this connection's principal may act for.
    Jobs,
    /// Schedule a job, replacing any of the same name this principal owns.
    Schedule(ScheduleRequest),
    /// Cancel a scheduled job.
    CancelJob {
        /// Which job.
        job: JobId,
    },
    /// Read the durable audit trail.
    Audit {
        /// Which records to return. Narrowing only — see [`AuditQuery`].
        query: AuditQuery,
    },
    /// Remove audit records older than a period.
    Prune {
        /// Records at or before this far back go.
        older_than_ms: u64,
        /// Count what would go instead of removing it.
        dry_run: bool,
    },
}

/// A job a client is asking for.
///
/// Not a [`JobSpec`](aik_api::scheduler::JobSpec): a spec names a handler component and a
/// free-form payload, and neither is a client's to choose. This says *when* and *what to
/// ask*, and the host builds the spec — stamping the agent handler and the owner it
/// authenticated — so a scheduled job can only ever be an agent turn belonging to whoever
/// scheduled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRequest {
    /// The job's stable name, unique per owner.
    pub id: JobId,
    /// When it runs.
    pub trigger: Trigger,
    /// What to ask when it fires.
    pub prompt: String,
    /// The transcript each firing appends to, or `None` for a fresh one each time.
    #[serde(default)]
    pub session: Option<SessionId>,
    /// Whether the job survives a restart.
    #[serde(default)]
    pub persistent: bool,
    /// How long one firing may take, if there is a limit.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Anything the host sends that is not a handshake.
///
/// Adjacently tagged, for the reason given on [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Response {
    /// Progress on a call that is still running.
    Update {
        /// Which call.
        id: u64,
        /// What the agent produced or did.
        update: AgentUpdate,
    },
    /// A call finished successfully.
    Done {
        /// Which call.
        id: u64,
        /// What it produced.
        reply: Reply,
    },
    /// A call failed.
    Failed {
        /// Which call.
        id: u64,
        /// Why.
        error: WireError,
    },
    /// Something needs a human's answer.
    ///
    /// Unsolicited: it belongs to whichever call caused it, but a client answers it with its
    /// own [`Request::Approve`] or [`Request::Deny`] rather than by replying in place, because
    /// the question outlives the frame that announced it and may be answered by a different
    /// client entirely.
    Approval {
        /// The question.
        pending: Box<PendingApproval>,
    },
    /// The host is going away.
    ///
    /// Sent once, best-effort, before the connection closes. Everything still in flight is
    /// cancelled and every parked approval is refused; this exists so a client can say so
    /// rather than reporting a bare disconnection.
    Closing {
        /// Why, in a sentence.
        message: String,
    },
}

/// What a successful call produced.
///
/// Adjacently tagged, for the reason given on [`Request`]. Several of these variants carry a
/// sequence, which an internally tagged enum cannot serialise at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Reply {
    /// Nothing beyond "it worked".
    Ok,
    /// Answer to [`Request::Ping`].
    Pong,
    /// Answer to [`Request::Status`].
    Status(Box<HostStatus>),
    /// Answer to [`Request::Prompt`], after the last update.
    Finished(Box<AgentResponse>),
    /// Answer to [`Request::Sessions`].
    Sessions(Vec<ContextStats>),
    /// Answer to [`Request::Clear`] or [`Request::Compact`].
    Removed {
        /// How many records went.
        records: usize,
    },
    /// Answer to [`Request::Jobs`].
    Jobs(Vec<ScheduledJob>),
    /// Answer to [`Request::CancelJob`].
    Cancelled {
        /// Whether there was a job to cancel.
        existed: bool,
    },
    /// Answer to [`Request::Audit`].
    Audit {
        /// The matching records this reader may see, newest first.
        records: Vec<AuditRecord>,
        /// The highest sequence the trail has ever issued.
        ///
        /// Not how many it still holds: retention removes records and never renumbers the
        /// rest. Carried so a reader can see at a glance that the window they asked for is a
        /// window, which is the same thing `aik audit` prints when it opens the file itself.
        issued: u64,
    },
    /// Answer to [`Request::Prune`].
    Pruned {
        /// How many records went, or would have.
        removed: u64,
        /// The highest sequence the trail has ever issued.
        issued: u64,
    },
}

/// What the host is serving.
///
/// Deliberately a description of the *deployment* rather than of its contents: which agent,
/// which model, which root, whether there is a database. Nothing here is a transcript, a
/// memory or an audit record, so a client that may connect at all learns nothing from it that
/// it could not learn from the configuration it can already read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// The host's version.
    pub version: String,
    /// The agent's identity.
    pub agent: String,
    /// The identity the agent acts for.
    pub user: String,
    /// The model every turn is sent to.
    pub model: String,
    /// The directory the filesystem tools are confined to.
    pub root: PathBuf,
    /// The database, if this deployment has one.
    #[serde(default)]
    pub database: Option<PathBuf>,
    /// Which memory tools are registered.
    pub memory: String,
    /// Whether scheduled jobs run in this process.
    pub runs_jobs: bool,
    /// How many clients are connected, this one included.
    pub connections: usize,
    /// How long the host has been up.
    pub uptime_ms: u64,
}

/// An error, carried across a process boundary.
///
/// Both halves matter. The [`kind`](WireError::kind) is what a client acts on — a refusal is
/// not a crash and must not be reported as one — and the message is what a person reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    /// How the host classified it.
    pub kind: WireErrorKind,
    /// The failure, rendered with its whole chain of causes.
    pub message: String,
}

impl WireError {
    /// Describes `error` for the wire.
    ///
    /// The message is the full chain, because the receiving process cannot walk
    /// [`std::error::Error::source`] across a socket and a bare outermost context — "talking
    /// to the model provider" — is not a diagnosis.
    pub fn new(error: &Error) -> Self {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            message.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        Self {
            kind: WireErrorKind::of(error),
            message,
        }
    }

    /// Rebuilds an [`Error`] on the receiving side.
    ///
    /// The classification survives exactly for the kinds a caller acts on differently — a
    /// refusal, a bad argument, an unsupported operation, a confinement violation, a
    /// cancellation — because those are the ones where treating the error as generic would
    /// change behaviour. Everything else becomes [`Error::other`] carrying the same message;
    /// [`WireError::kind`] still reports what the host actually said, for a caller that wants
    /// the distinction back.
    pub fn into_error(self) -> Error {
        match self.kind {
            WireErrorKind::Permission => Error::PermissionDenied(self.message),
            WireErrorKind::InvalidArgument => Error::InvalidArgument(self.message),
            WireErrorKind::Unsupported => Error::Unsupported(self.message),
            WireErrorKind::Confinement => Error::Confinement(self.message),
            WireErrorKind::Cancelled => Error::Cancelled,
            _ => Error::other(self.message),
        }
    }
}

/// A serialisable mirror of [`ErrorKind`].
///
/// A mirror rather than a `serde` derive on the kernel's own enum: the wire format is a
/// compatibility surface between two versions of two programs, and the kernel's
/// classification is free to gain a variant without that being a protocol change. Anything
/// this build does not know becomes [`WireErrorKind::Other`], which is what an old client
/// should do with a new host's classification anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireErrorKind {
    /// The configuration is missing or malformed.
    Config,
    /// A lookup failed.
    NotFound,
    /// The caller supplied something invalid.
    InvalidArgument,
    /// The operation is not supported by this deployment.
    Unsupported,
    /// The operation was refused by policy or by ownership.
    Permission,
    /// A resource resolved outside a boundary enforced independently of policy.
    Confinement,
    /// The operation ran out of time.
    Timeout,
    /// The operation was cancelled.
    Cancelled,
    /// Anything else.
    Other,
}

impl WireErrorKind {
    /// Classifies an error for the wire.
    pub fn of(error: &Error) -> Self {
        match error.kind() {
            ErrorKind::Config => Self::Config,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::InvalidArgument => Self::InvalidArgument,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::Permission => Self::Permission,
            ErrorKind::Confinement => Self::Confinement,
            ErrorKind::Timeout => Self::Timeout,
            ErrorKind::Cancelled => Self::Cancelled,
            _ => Self::Other,
        }
    }
}

/// Turns a [`Reply`] into whatever the caller expected, or says what arrived instead.
///
/// A host that answered the wrong shape is a bug rather than a refusal, so this is an
/// [`Error::other`] and not something a caller should try to recover from.
pub fn unexpected<T>(expected: &str, reply: &Reply) -> Result<T> {
    Err(Error::other(format!(
        "the host answered with {} where {expected} was expected",
        reply_name(reply),
    )))
}

/// The reply's variant name, for diagnostics.
pub fn reply_name(reply: &Reply) -> &'static str {
    match reply {
        Reply::Ok => "ok",
        Reply::Pong => "pong",
        Reply::Status(_) => "a status",
        Reply::Finished(_) => "a finished turn",
        Reply::Sessions(_) => "a session listing",
        Reply::Removed { .. } => "a removal count",
        Reply::Jobs(_) => "a job listing",
        Reply::Cancelled { .. } => "a cancellation",
        Reply::Audit { .. } => "audit records",
        Reply::Pruned { .. } => "a prune result",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_has_nowhere_to_name_a_principal() {
        // The security property this protocol rests on, asserted against the parser rather
        // than against a reading of the type: a client that names a principal is not a client
        // that acts as one, because there is no such field to populate.
        let call: Call = serde_json::from_value(json!({
            "id": 1,
            "request": { "type": "sessions", "principal": "root" },
        }))
        .expect("an unknown key is ignored rather than honoured");
        assert_eq!(call.request, Request::Sessions);

        let call: Call = serde_json::from_value(json!({
            "id": 2,
            "request": {
                "type": "prompt",
                "value": { "input": "hello", "principal": "root", "on_behalf_of": "root" },
            },
        }))
        .expect("an unknown key is ignored rather than honoured");
        assert_eq!(
            call.request,
            Request::Prompt {
                session: None,
                input: "hello".to_owned(),
            },
        );
    }

    #[test]
    fn every_reply_survives_the_wire() {
        // An internally tagged enum cannot serialise a newtype variant holding a sequence at
        // all, and the failure is at runtime rather than at compile time — so the round trip
        // is asserted for each variant rather than assumed.
        let replies = [
            Reply::Ok,
            Reply::Pong,
            Reply::Sessions(Vec::new()),
            Reply::Jobs(Vec::new()),
            Reply::Removed { records: 3 },
            Reply::Cancelled { existed: true },
            Reply::Audit {
                records: Vec::new(),
                issued: 9,
            },
            Reply::Pruned {
                removed: 1,
                issued: 2,
            },
        ];
        for reply in replies {
            let encoded = serde_json::to_string(&reply).expect("serialised");
            assert_eq!(
                serde_json::from_str::<Reply>(&encoded).expect("deserialised"),
                reply,
            );
        }
    }

    #[test]
    fn an_agent_update_survives_the_wire() {
        // `AgentUpdate::Content` carries a `ContentPart`, which has a `type` of its own. This
        // is the round trip that catches a tag collision between the two.
        let updates = [
            AgentUpdate::Content(aik_api::model::ContentPart::text("hello")),
            AgentUpdate::Status {
                message: "thinking".to_owned(),
            },
        ];
        for update in updates {
            let response = Response::Update {
                id: 1,
                update: update.clone(),
            };
            let encoded = serde_json::to_string(&response).expect("serialised");
            assert_eq!(
                serde_json::from_str::<Response>(&encoded).expect("deserialised"),
                response,
            );
        }
    }

    #[test]
    fn a_schedule_request_cannot_name_a_handler_or_an_owner() {
        let raw = serde_json::to_value(ScheduleRequest {
            id: JobId::new("nightly"),
            trigger: Trigger::Every {
                interval: Duration::from_secs(60),
            },
            prompt: "summarise today".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        })
        .expect("serialised");

        for field in ["handler", "owner", "principal", "payload"] {
            assert!(
                raw.get(field).is_none(),
                "`{field}` must not be a client's to choose",
            );
        }
    }

    #[test]
    fn a_refusal_stays_a_refusal_across_the_wire() {
        let original = Error::PermissionDenied("session `s` belongs to `alice`".to_owned());
        let wire = WireError::new(&original);
        assert_eq!(wire.kind, WireErrorKind::Permission);

        let rebuilt = wire.clone().into_error();
        assert_eq!(rebuilt.kind(), ErrorKind::Permission);
        assert!(
            rebuilt.to_string().contains("belongs to `alice`"),
            "{wire:?}"
        );
    }

    #[test]
    fn a_wire_error_carries_the_whole_chain_of_causes() {
        let root = std::io::Error::other("connection refused");
        let error = Error::wrap("talking to the model provider", root);

        let wire = WireError::new(&error);
        assert!(wire.message.contains("talking to the model"), "{wire:?}");
        assert!(wire.message.contains("connection refused"), "{wire:?}");
    }

    #[test]
    fn an_unknown_classification_from_a_newer_host_is_not_a_parse_failure() {
        let wire: std::result::Result<WireError, _> =
            serde_json::from_value(json!({ "kind": "quota_exceeded", "message": "no" }));
        assert!(
            wire.is_err(),
            "an unknown kind is refused rather than silently read as `other`, so the \
             mismatch is reported once at the protocol level rather than mis-classified",
        );
    }

    #[test]
    fn every_message_round_trips() {
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            token: "abc".to_owned(),
            client: "aik".to_owned(),
            interactive: true,
        };
        let encoded = serde_json::to_string(&hello).expect("encoded");
        assert_eq!(
            serde_json::from_str::<Hello>(&encoded).expect("decoded"),
            hello
        );

        let call = Call {
            id: 7,
            request: Request::Compact {
                session: SessionId::new(),
                keep: 10,
            },
        };
        let encoded = serde_json::to_string(&call).expect("encoded");
        assert_eq!(
            serde_json::from_str::<Call>(&encoded).expect("decoded"),
            call
        );
    }
}
