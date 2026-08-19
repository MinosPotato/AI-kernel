//! A human-in-the-loop [`ApprovalSink`](aik_api::permission::ApprovalSink).
//!
//! [`aik-policy`](https://docs.rs/aik-policy) can already answer
//! [`Decision::RequireApproval`](aik_api::permission::Decision::RequireApproval), and
//! [`aik-tools`](https://docs.rs/aik-tools) already resolves that decision through an
//! `ApprovalSink` — but until now nothing implemented one, so every rule that asked for a
//! human was, in practice, a denial. This crate is the missing half: the mechanism by which
//! a question reaches a person and their answer reaches the authorization check that is
//! waiting for it.
//!
//! It contains no user interface. [`ApprovalBroker`] is a rendezvous point: the
//! authorization path parks a question on it and waits; a frontend — a CLI prompt, a
//! desktop popup, a chat bridge — takes questions off it through an [`ApprovalGate`] and
//! answers them. That split is what lets one implementation serve every frontend the system
//! will grow, and it is why the broker can be tested exhaustively without a terminal.
//!
//! ```no_run
//! use std::sync::Arc;
//! use aik_approval::{ApprovalBroker, ApprovalComponent};
//! use aik_api::permission::ApprovalSink;
//! use aik_core::prelude::*;
//! use aik_tools::ToolsComponent;
//!
//! # fn build(policy: Arc<dyn aik_api::permission::PolicyEngine>) -> Result<Kernel> {
//! let broker = Arc::new(ApprovalBroker::new());
//!
//! let kernel = Kernel::builder()
//!     // Publishes the broker so a frontend component can attach a gate to it.
//!     .component(ApprovalComponent::new(broker.clone()))
//!     .component(
//!         ToolsComponent::new()
//!             .with_policy(policy)
//!             .with_approvals(broker as Arc<dyn ApprovalSink>),
//!     )
//!     .build()?;
//! # Ok(kernel)
//! # }
//! ```
//!
//! # The two sides are deliberately different types
//!
//! Asking and answering are separate capabilities, so they are separate handles.
//! [`ApprovalBroker`] is an `ApprovalSink`: all it can do is pose a question and wait.
//! [`ApprovalGate`] is the answering side, and holding one is what makes a deployment
//! capable of granting anything at all.
//!
//! An [`ApprovalGate`] is a **trusted capability**: whoever holds one can approve any
//! request, including a request made by the agent whose behaviour the approval was supposed
//! to constrain. It must only ever be given to the frontend that actually asks a human, and
//! never to an agent, a tool, or a model. This is the same rule the
//! [`aik_api::tool`] module states for `dyn Tool`, for the same reason: a capability that
//! can be reached by untrusted code is not a boundary.
//!
//! # Failing closed
//!
//! Every way this mechanism can fail to produce an answer is an error, never `Ok(true)`,
//! and — because [`ApprovalSink`](aik_api::permission::ApprovalSink) implementations report
//! only "granted" or "not granted" — never a silent success:
//!
//! | Situation | Result |
//! |---|---|
//! | A human approved | `Ok(true)` |
//! | A human refused | `Ok(false)` |
//! | No gate is attached, so nobody can answer | [`Error::PermissionDenied`](aik_core::Error::PermissionDenied), immediately |
//! | More requests are already waiting than [`ApprovalSettings::max_pending`] | [`Error::PermissionDenied`](aik_core::Error::PermissionDenied), immediately |
//! | Nobody answered in time | [`Error::Timeout`](aik_core::Error::Timeout) |
//! | The operation was cancelled | [`Error::Cancelled`](aik_core::Error::Cancelled) |
//! | The broker was closed, e.g. by shutdown | [`Error::Cancelled`](aik_core::Error::Cancelled) |
//! | An answer arrived after the requester gave up | it has no effect; the responder is told |
//!
//! The gate count is what turns "nobody can answer" into an immediate refusal rather than a
//! long wait: a headless deployment that wires a broker but never attaches a frontend
//! behaves exactly like one that configured no approval sink at all, which is what
//! [`aik_api::permission`] says it should. Holding an [`ApprovalGate`] is therefore an
//! assertion — "I will put these to a human" — and dropping it withdraws that assertion.
//!
//! # What this crate does not do
//!
//! * **It does not decide.** It carries a question and an answer. What is worth asking about
//!   is a policy question, answered by a `PolicyEngine`.
//! * **It does not remember.** Every question is asked afresh; there is no "always allow".
//!   Persisting an answer means widening a grant beyond the operation it was given for, and
//!   that belongs in policy, where it can be reviewed, not in the prompt that bypassed it.
//! * **It does not authenticate the answerer.** The system has no user identity model yet,
//!   so an audit record says an approval was granted but not by whom.
//! * **It does not persist.** Pending approvals live in memory and are refused on shutdown;
//!   a question that outlives the process would be answering for an operation that no longer
//!   exists.

mod broker;
mod component;
mod gate;

pub use broker::{
    ApprovalBroker, ApprovalSettings, DEFAULT_MAX_PENDING, DEFAULT_TIMEOUT, NOTIFICATION_CAPACITY,
};
pub use component::{ApprovalComponent, DEFAULT_COMPONENT_ID};
pub use gate::{ApprovalGate, ApprovalId, ApprovalStream, PendingApproval};
