//! Persistent memory contracts.
//!
//! A [`MemoryStore`] holds records that outlive a conversation: facts, preferences,
//! summaries, observations. The record shape is intentionally thin — an id, a kind, JSON
//! content, metadata, timestamps and an optional embedding — because the interesting part
//! (what to remember, when, and how to retrieve it) is policy that belongs in a memory
//! subsystem, not in the storage contract.
//!
//! [`MemoryQuery`] supports lookup by kind, by metadata and by semantic similarity, since
//! any real store will need all three; a backend that cannot do one of them should say so
//! with [`Error::Unsupported`](aik_core::Error::Unsupported) rather than silently ignore
//! it.
//!
//! # Records are owned
//!
//! A record belongs to the principal whose [`ExecutionContext`] first stored it, exactly as a
//! [context session](crate::context#what-the-model-can-and-cannot-touch) belongs to the
//! principal that first appended to it. [`MemoryRecord::owner`] is stamped by the store from
//! the context and is never taken from the record the caller passed in, so it cannot be
//! forged by whatever produced the content.
//!
//! Every method enforces it, in the way that fits what the method does:
//!
//! * [`MemoryStore::put`], [`MemoryStore::get`] and [`MemoryStore::delete`] name one record,
//!   so a caller that may not act for its owner is refused with
//!   [`Error::PermissionDenied`](aik_core::Error::PermissionDenied) rather than told the
//!   record is absent.
//! * [`MemoryStore::query`] enumerates, so records the caller may not act for are simply not
//!   in the results — the same shape as the expiry filter, and for the same reason: an
//!   enumeration that errored because something it was not asking for exists would leak that
//!   it exists.
//!
//! [`Principal::may_act_for`](crate::permission::Principal::may_act_for) is the single
//! definition of "may act for", shared with the context store, so the two cannot drift.

use aik_core::Result;
use aik_core::clock::Timestamp;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::execution::ExecutionContext;
use crate::permission::{Principal, PrincipalId};

aik_core::uuid_id! {
    /// Identifies one memory record.
    pub MemoryId
}

aik_core::string_id! {
    /// Classifies a record, e.g. `fact`, `preference`, `summary`.
    ///
    /// Kinds are how a memory subsystem keeps different retention and retrieval policies
    /// apart without needing separate stores.
    pub MemoryKind
}

/// One stored memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// The record's identifier.
    pub id: MemoryId,
    /// The principal this memory belongs to.
    ///
    /// Assigned by the store from the [`ExecutionContext`], never from the record handed to
    /// [`MemoryStore::put`] — so whatever a caller sets here is overwritten, and content a
    /// model produced can never choose whose memory it becomes. [`MemoryRecord::new`]
    /// therefore leaves it as the system principal, and a caller that wants to know what was
    /// actually recorded reads it back with [`MemoryStore::get`].
    pub owner: PrincipalId,
    /// What kind of memory it is.
    pub kind: MemoryKind,
    /// The memory itself.
    pub content: Value,
    /// Filterable annotations: source, subject, confidence, tags.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it should be forgotten, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// A vector representation, for semantic retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl MemoryRecord {
    /// Creates a record with a fresh id and no metadata.
    ///
    /// [`MemoryRecord::owner`] is left as the system principal and is replaced by whichever
    /// principal actually stores it; see that field for why the caller does not get to choose.
    pub fn new(
        kind: impl Into<MemoryKind>,
        content: impl Into<Value>,
        created_at: Timestamp,
    ) -> Self {
        Self {
            id: MemoryId::new(),
            owner: PrincipalId::new(Principal::SYSTEM),
            kind: kind.into(),
            content: content.into(),
            metadata: Map::new(),
            created_at,
            expires_at: None,
            embedding: None,
        }
    }
}

/// What to retrieve.
///
/// Fields combine conjunctively: a query with a kind and a text matches records of that
/// kind, ranked by similarity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryQuery {
    /// Restrict to these kinds. Empty means all kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<MemoryKind>,
    /// Semantic search text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A pre-computed embedding, when the caller has already embedded the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    /// Metadata keys that must match exactly.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Maximum number of results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Minimum similarity, for semantic queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f32>,
}

/// A record and how well it matched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryMatch {
    /// The record.
    pub record: MemoryRecord,
    /// Similarity, for semantic queries. `None` for exact lookups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// Somewhere memories live.
///
/// # Ownership
///
/// A record belongs to the principal that first stored it, and every method must reject a
/// caller that is neither that principal nor one acting
/// [`on_behalf_of`](crate::permission::Principal::on_behalf_of) it — see the
/// [module documentation](self#records-are-owned). A context with no principal is the system
/// acting for itself and gets its own identity, not a wildcard.
#[async_trait]
pub trait MemoryStore: Send + Sync + 'static {
    /// Stores a record, replacing any existing one with the same id.
    ///
    /// The stored [`MemoryRecord::owner`] is taken from `cx`, whatever the passed record
    /// says. Replacing a record that belongs to another principal is
    /// [`Error::PermissionDenied`](aik_core::Error::PermissionDenied), not a silent
    /// overwrite: an upsert that could take a record away from its owner would make an id
    /// collision a privilege escalation.
    async fn put(&self, record: MemoryRecord, cx: &ExecutionContext) -> Result<()>;

    /// Fetches one record.
    ///
    /// `Ok(None)` if no such record exists;
    /// [`Error::PermissionDenied`](aik_core::Error::PermissionDenied) if one does and it
    /// belongs to someone else.
    async fn get(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<Option<MemoryRecord>>;

    /// Deletes one record, returning whether it existed.
    ///
    /// Refuses another principal's record rather than deleting it, on the same terms as
    /// [`MemoryStore::get`].
    async fn delete(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<bool>;

    /// Retrieves matching records, best first.
    ///
    /// Only records the caller may act for are considered. Another principal's records are
    /// absent from the results rather than an error, so a query cannot be used to discover
    /// that they exist.
    async fn query(&self, query: &MemoryQuery, cx: &ExecutionContext) -> Result<Vec<MemoryMatch>>;
}
