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
//! # What this deliberately does not do
//!
//! * **It does not rank by meaning.** [`MemoryQuery`](aik_api::memory::MemoryQuery) can
//!   carry search text or a pre-computed embedding, and [`MemoryRecord`] can carry one too,
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
//! * **It does not scope records to a principal.** Unlike a context session, a
//!   [`MemoryRecord`](aik_api::memory::MemoryRecord) carries no owner, and the trait carries
//!   no session to check one against. Anything that must not leak between users has to
//!   enforce that above this crate, e.g. by namespacing kinds or ids per principal. The
//!   `cx` parameter every method takes is therefore unused today; it exists so this trait
//!   can grow attribution or audit reporting later — as [`ContextStore`](aik_api::context)
//!   did — without changing every implementation's signature again.

mod component;
mod expiry;
mod persistent;
mod query;
mod store;

pub use component::{DEFAULT_COMPONENT_ID, DEFAULT_EXPIRY_SWEEP_INTERVAL, MemoryComponent, RedbMemoryComponent};
pub use expiry::ExpirySweeper;
pub use persistent::RedbMemoryStore;
pub use store::InMemoryMemoryStore;
