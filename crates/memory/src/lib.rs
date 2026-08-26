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
//! ## What an embedding model sees
//!
//! Configuring one changes where memory content goes. Every record's content is sent to the
//! [`Embedder`](aik_api::model::Embedder) when it is stored, and every search text when it is
//! run — so a store wired to a remote embedding service sends it everything anyone remembers,
//! including whatever the filesystem tools read into a memory. With the workspace's only
//! `Embedder` that means a local Ollama server by default, and it is a deployment's decision
//! rather than this crate's: the store is *given* an embedder, and what is behind it is
//! whatever the kernel published.
//!
//! Ownership is unaffected in both directions. Embedding happens before the owner is even
//! consulted, so it cannot leak one principal's records to another; and similarity ranks the
//! records a caller may already act for, never widening that set — a semantic query filters
//! by owner exactly as an exact one does.
//!
//! # Searching by meaning
//!
//! Both stores rank by similarity, and both do it the same way, because the ranking lives in
//! one place they share. What differs is only what a given store was *given*:
//!
//! * **A pre-computed vector always works.** A
//!   [`MemoryQuery::embedding`](aik_api::memory::MemoryQuery::embedding) is compared against
//!   the vectors on the records by cosine similarity, and
//!   [`MemoryQuery::min_score`](aik_api::memory::MemoryQuery::min_score) drops what falls
//!   below it. That is arithmetic over what is already stored, so it needs no model at all.
//! * **Searching by text needs one.** A store built with
//!   [`InMemoryMemoryStore::with_embedder`] or [`RedbMemoryStore::with_embedder`] embeds
//!   every record it stores and every
//!   [`MemoryQuery::text`](aik_api::memory::MemoryQuery::text) it is given. A store without
//!   one answers text with [`Error::Unsupported`](aik_core::Error::Unsupported) — never with
//!   the most recent records instead, which would read as a memory that had forgotten rather
//!   than as one that cannot search — and says so in advance through
//!   [`MemoryStore::capabilities`](aik_api::memory::MemoryStore::capabilities).
//!
//! Three consequences worth knowing before turning it on:
//!
//! * A write that cannot embed **fails**. Storing a record with no vector would make it
//!   invisible to every future search, silently, for as long as it lives.
//! * Records written before an embedding model was configured keep no vector, and are absent
//!   from semantic results while remaining exactly retrievable. Nothing back-fills them:
//!   re-embedding a store is a migration with its own cost.
//! * Changing the embedding model invalidates every vector already stored. Similarity across
//!   two models is not defined, so records embedded by the old one are skipped rather than
//!   compared — see [`MemoryStore::query`](aik_api::memory::MemoryStore::query).
//!
//! There is no approximate-nearest-neighbour index behind any of this: a query scans the
//! records the exact filters already narrowed it to. That is the right shape for a personal
//! record store and the wrong one for a corpus, and it is the thing to change first if this
//! ever holds enough records for the scan to show.
//!
//! # What this deliberately does not do
//!
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
mod semantic;
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
