//! A durable, append-only record of what this system was allowed to do.
//!
//! [`ToolRegistry`](aik_api::tool::ToolRegistry) already answers every "may this happen?" in
//! one place and publishes the answer as an event. That makes a run *observable* while it is
//! happening, which is not the same as accountable: a terminal that has scrolled, a process
//! that has exited and a subscriber that was never attached all leave the same gap. This
//! crate closes it, by writing those events down and keeping them.
//!
//! # What this crate contains
//!
//! * [`InMemoryAuditStore`] — the reference [`AuditStore`](aik_api::audit::AuditStore): the
//!   trail for as long as the process lives.
//! * [`RedbAuditStore`] — the same contract in the kernel's shared database. Both run the
//!   same conformance suite, because "persistent" must not mean "subtly different".
//! * [`AuditSink`] — the subscriber that turns published events into stored records, and
//!   turns the bus dropping events into a recorded [gap](aik_api::audit::AuditGap).
//! * [`AuditComponent`] and [`RedbAuditComponent`] — the kernel wiring for each, including
//!   the optional background retention sweep.
//! * [`AuditRetentionSweeper`] — the only way anything is ever removed, and a capability
//!   deliberately separate from the store itself.
//!
//! ```
//! use aik_api::audit::{AuditEntry, AuditGap, AuditQuery, AuditStore};
//! use aik_api::execution::ExecutionContext;
//! use aik_audit::InMemoryAuditStore;
//! use aik_core::clock::Timestamp;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> aik_core::Result<()> {
//! let store = InMemoryAuditStore::new();
//! store
//!     .append(AuditEntry::Gap(AuditGap {
//!         timestamp: Timestamp::from_millis(1),
//!         missed: 3,
//!     }))
//!     .await?;
//!
//! let found = store.query(&AuditQuery::default(), &ExecutionContext::new()).await?;
//! assert_eq!(found.len(), 1);
//! assert_eq!(found[0].sequence, 1);
//! # Ok(())
//! # }
//! ```
//!
//! # Security
//!
//! Four properties, in the order they matter:
//!
//! * **No model can reach it.** There is no audit tool, and there will not be one. Every
//!   other durable subsystem here exposes itself to an agent through the tool registry
//!   precisely so that policy is consulted; the audit trail is the thing that records those
//!   consultations, and a model able to read it could learn where the boundaries are, while
//!   one able to write to it could describe its own behaviour. Reviewing the trail is an
//!   operator action, through the CLI, against a file only the operator can open.
//! * **Append-only, structurally.** [`AuditStore`](aik_api::audit::AuditStore) has no update
//!   and no delete. Removal exists only behind [`AuditRetentionSweeper`], which is a separate
//!   trait held by the retention task and the operator's explicit prune — so holding the
//!   store gives no ability to erase.
//! * **Reads are authorized, and filters are not authorization.** A query returns only what
//!   [`AuditRecord::visible_to`](aik_api::audit::AuditRecord::visible_to) allows: what a
//!   principal did, and what was done on their behalf. Naming somebody else in a filter
//!   narrows that; it never widens it. Records the reader may not see are absent rather than
//!   an error, because an error would confirm they exist.
//! * **Loss is recorded, never silent.** Events the bus dropped become an
//!   [`AuditGap`](aik_api::audit::AuditGap); records retention removed become a
//!   [`RetentionApplied`](aik_api::audit::RetentionApplied). Both are visible to every reader,
//!   are never hidden by a principal filter, and are never swept — so no identity and no
//!   question about *who* did something can make a short trail look complete. (Asking for one
//!   kind or one operation does exclude them, because that is what was asked for.) A trail may
//!   be incomplete; it may not lie about being complete.
//!
//! # What it deliberately does not do
//!
//! * **It does not gate anything.** The write happens after the decision, on a subscriber, so
//!   a store that cannot be written to does not refuse the tool call — it logs, counts, and
//!   leaves the trail short. Auditing synchronously would mean putting a disk inside
//!   `ToolRegistry::invoke`, which is a change to the enforcement point rather than to the
//!   trail; see [`sink`] for the full argument.
//! * **It does not record arguments or output.** It stores the published events verbatim, and
//!   [those carry the shape of what happened, never its contents](aik_api::audit).
//! * **It does not sign or chain records.** Sequence numbers make a removal visible to
//!   anything that reads the trail in order, but nothing here defends against an attacker who
//!   can already write to the database file — that is what the file's `0600` mode and, in a
//!   deployment that needs more, shipping records off the host are for.

mod component;
mod persistent;
pub mod retention;
mod settings;
pub mod sink;
mod store;

pub use component::{AuditComponent, DEFAULT_COMPONENT_ID, RedbAuditComponent};
pub use persistent::RedbAuditStore;
pub use retention::{
    AuditRetentionSweeper, DEFAULT_RETENTION_BATCH, DEFAULT_RETENTION_SWEEP_INTERVAL,
};
pub use settings::AuditSettings;
pub use sink::AuditSink;
pub use store::InMemoryAuditStore;
