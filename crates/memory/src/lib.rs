//! A durable place for the facts an agent should not have to re-derive every turn.
//!
//! [`ContextStore`](aik_api::context::ContextStore) is the transcript: everything that was
//! said, kept in order, for one conversation. It is deliberately not where an agent keeps
//! anything meant to outlive that conversation — a user's stated preference, a fact learned
//! while working, a summary worth keeping once the turns that produced it are gone. That is
//! [`MemoryStore`](aik_api::memory::MemoryStore): unordered, keyed by id, retrieved by kind
//! or by metadata rather than by position in a transcript.
//!
//! # What this crate contains
//!
//! * [`InMemoryMemoryStore`] — the reference [`MemoryStore`](aik_api::memory::MemoryStore):
//!   a map, guarded by a lock, gone at the next restart.
//! * [`RedbMemoryStore`] — the same contract kept in the kernel's shared database. Both run
//!   the same conformance tests, because "persistent" must not mean "subtly different".
//! * [`MemoryComponent`] and [`RedbMemoryComponent`] — the kernel wiring for each, including
//!   the background sweep that reclaims expired records.
//! * [`tools`] — the four tools that put memory within an agent's reach, and the component
//!   that binds them to whichever store the kernel published. They are how an agent stores
//!   and recalls anything: through the tool registry, so that policy is consulted and the
//!   owner is the principal the run is for. See that module for why that is the only path
//!   offered.
//!
//! ```
//! use aik_api::execution::ExecutionContext;
//! use aik_api::memory::{MemoryRecord, MemoryStore};
//! use aik_core::clock::Timestamp;
//! use aik_memory::InMemoryMemoryStore;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> aik_core::Result<()> {
//! let store = InMemoryMemoryStore::new();
//! let cx = ExecutionContext::new();
//!
//! let record = MemoryRecord::new("preference", serde_json::json!({"theme": "dark"}), Timestamp::now());
//! let id = record.id;
//! store.put(record, &cx).await?;
//!
//! assert!(store.get(&id, &cx).await?.is_some());
//! # Ok(())
//! # }
//! ```
//!
//! # Security
//!
//! A record belongs to the principal that stored it. The reasoning is with the ownership
//! rules themselves;
//! the short version is that the owner is taken from the
//! [`ExecutionContext`](aik_api::execution::ExecutionContext) and never from the record, so
//! content a model produced cannot choose whose memory it becomes, and a replacement keeps
//! the owner it already had, so an agent revising a memory on its principal's behalf does not
//! thereby acquire it.
//!
//! Naming another principal's record is
//! [`Error::PermissionDenied`](aik_core::Error::PermissionDenied); enumerating simply does
//! not return it, because an error would confirm it exists. The rule itself is
//! [`Principal::may_act_for`](aik_api::permission::Principal::may_act_for), shared with the
//! context store so the two subsystems cannot answer the same question differently.
//!
//! The sweep is the one operation that is not principal-scoped, deliberately: retention is a
//! property of the record, and housekeeping that could only reclaim the records of whoever
//! happened to trigger it would leave everyone else's expired data on disk indefinitely.
//!
//! # What this deliberately does not do
//!
//! * **It does not rank by meaning.** [`MemoryQuery`](aik_api::memory::MemoryQuery) can
//!   carry search text or a pre-computed embedding, and [`MemoryRecord`](aik_api::memory::MemoryRecord) can carry one too,
//!   but nothing here compares them. Semantic retrieval needs an
//!   [`Embedder`](aik_api::model::Embedder) and an index built for approximate nearest
//!   neighbours — real infrastructure with its own cost and failure modes, not something to
//!   bolt on silently. A query that asks for it gets
//!   [`Error::Unsupported`](aik_core::Error::Unsupported), never a result that quietly
//!   ignored the request. What both stores answer today is exact: by id, by kind, by
//!   metadata equality.
//! * **It does not decide what is worth remembering.** That policy — what to write, when to
//!   forget it early, how to reconcile two records about the same fact — belongs to whatever
//!   calls [`MemoryStore::put`](aik_api::memory::MemoryStore::put), not to the store.
//! * **It does not decide retention policy.** Records expire if something set
//!   `expires_at`; nothing here decides what that should be for a given kind, because that
//!   is a judgement about the value of a memory rather than about storing one.
//! * **It does not remember or recall on its own.** Nothing watches a conversation for facts
//!   worth keeping, and nothing slips recalled records into a prompt. A memory is written
//!   when something asks for it to be written — through [`tools`], that means a model asked
//!   and a policy agreed — so every memory in the store is one an audit trail can account
//!   for.

mod component;
mod expiry;
mod owner;
mod persistent;
mod query;
mod store;
pub mod tools;

pub use component::{
    DEFAULT_COMPONENT_ID, DEFAULT_EXPIRY_SWEEP_INTERVAL, MemoryComponent, RedbMemoryComponent,
};
pub use expiry::{DEFAULT_SWEEP_BATCH, ExpirySweeper};
pub use persistent::RedbMemoryStore;
pub use store::InMemoryMemoryStore;
pub use tools::{
    DEFAULT_TOOLS_COMPONENT_ID, MemoryDeleteTool, MemoryGetTool, MemoryPutTool, MemoryQueryTool,
    MemoryToolsComponent,
};
