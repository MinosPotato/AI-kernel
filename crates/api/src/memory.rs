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

use aik_core::Result;
use aik_core::clock::Timestamp;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::execution::ExecutionContext;

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
    pub fn new(
        kind: impl Into<MemoryKind>,
        content: impl Into<Value>,
        created_at: Timestamp,
    ) -> Self {
        Self {
            id: MemoryId::new(),
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
#[async_trait]
pub trait MemoryStore: Send + Sync + 'static {
    /// Stores a record, replacing any existing one with the same id.
    async fn put(&self, record: MemoryRecord, cx: &ExecutionContext) -> Result<()>;

    /// Fetches one record.
    async fn get(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<Option<MemoryRecord>>;

    /// Deletes one record, returning whether it existed.
    async fn delete(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<bool>;

    /// Retrieves matching records, best first.
    async fn query(&self, query: &MemoryQuery, cx: &ExecutionContext) -> Result<Vec<MemoryMatch>>;
}
