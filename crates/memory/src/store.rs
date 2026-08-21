//! [`InMemoryMemoryStore`]: the reference [`MemoryStore`] implementation.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryId, MemoryMatch, MemoryQuery, MemoryRecord, MemoryStore};
use aik_core::Result;
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use async_trait::async_trait;

use crate::expiry::{ExpirySweeper, is_live};
use crate::query::{matches_metadata, rank, reject_unsupported, requested_kinds};

/// A [`MemoryStore`] that keeps records in memory, in this process.
///
/// # What it guarantees
///
/// * **Upsert by id.** [`MemoryStore::put`] inserts or replaces whole-record, atomically
///   with respect to any concurrent reader: a query never observes half of a replacement.
/// * **Exact retrieval.** [`MemoryStore::get`] and [`MemoryStore::delete`] address a record
///   by id and do not apply the expiry filter below — see [`crate::expiry`] for why.
/// * **Live-only queries.** [`MemoryStore::query`] never returns a record whose `expires_at`
///   is at or before the store's clock, whether or not the periodic sweep has reached it yet.
///
/// # What it does not
///
/// It does not persist. A restart loses every record, which is correct for a reference
/// implementation: the [`MemoryStore`] contract is the seam [`RedbMemoryStore`](crate::RedbMemoryStore)
/// slots into with nothing else changing.
pub struct InMemoryMemoryStore {
    records: RwLock<HashMap<MemoryId, MemoryRecord>>,
    clock: SharedClock,
}

impl std::fmt::Debug for InMemoryMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let records = self.records.read().expect("memory store lock poisoned");
        f.debug_struct("InMemoryMemoryStore")
            .field("records", &records.len())
            .finish()
    }
}

impl Default for InMemoryMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryMemoryStore {
    /// Creates an empty store using the system clock.
    pub fn new() -> Self {
        Self {
            records: RwLock::new(HashMap::new()),
            clock: Arc::new(SystemClock),
        }
    }

    /// Overrides the clock used to decide which records are live. Defaults to the system
    /// clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn put(&self, record: MemoryRecord, _cx: &ExecutionContext) -> Result<()> {
        let mut records = self.records.write().expect("memory store lock poisoned");
        records.insert(record.id, record);
        Ok(())
    }

    async fn get(&self, id: &MemoryId, _cx: &ExecutionContext) -> Result<Option<MemoryRecord>> {
        let records = self.records.read().expect("memory store lock poisoned");
        Ok(records.get(id).cloned())
    }

    async fn delete(&self, id: &MemoryId, _cx: &ExecutionContext) -> Result<bool> {
        let mut records = self.records.write().expect("memory store lock poisoned");
        Ok(records.remove(id).is_some())
    }

    async fn query(&self, query: &MemoryQuery, _cx: &ExecutionContext) -> Result<Vec<MemoryMatch>> {
        reject_unsupported(query)?;
        let kinds = requested_kinds(query);
        let now = self.clock.now();

        let records = self.records.read().expect("memory store lock poisoned");
        let candidates: Vec<MemoryRecord> = records
            .values()
            .filter(|record| kinds.is_empty() || kinds.contains(&record.kind))
            .filter(|record| is_live(record.expires_at, now))
            .filter(|record| matches_metadata(record, &query.metadata))
            .cloned()
            .collect();

        Ok(rank(candidates, query.limit))
    }
}

#[async_trait]
impl ExpirySweeper for InMemoryMemoryStore {
    async fn sweep_expired(&self, now: Timestamp) -> Result<usize> {
        let mut records = self.records.write().expect("memory store lock poisoned");
        let before = records.len();
        records.retain(|_, record| is_live(record.expires_at, now));
        Ok(before - records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{Principal, PrincipalKind};
    use aik_core::ErrorKind;
    use aik_core::clock::ManualClock;
    use serde_json::json;

    fn cx() -> ExecutionContext {
        ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
    }

    fn record(kind: &str) -> MemoryRecord {
        MemoryRecord::new(kind, json!({"n": 1}), Timestamp::from_millis(1_000))
    }

    #[tokio::test]
    async fn a_missing_id_is_none() {
        let store = InMemoryMemoryStore::new();
        assert!(store.get(&MemoryId::new(), &cx()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let store = InMemoryMemoryStore::new();
        let record = record("fact");
        store.put(record.clone(), &cx()).await.unwrap();
        assert_eq!(store.get(&record.id, &cx()).await.unwrap(), Some(record));
    }

    #[tokio::test]
    async fn put_upserts_by_id() {
        let store = InMemoryMemoryStore::new();
        let mut record = record("fact");
        store.put(record.clone(), &cx()).await.unwrap();

        record.content = json!({"n": 2});
        store.put(record.clone(), &cx()).await.unwrap();

        assert_eq!(
            store.get(&record.id, &cx()).await.unwrap().unwrap().content,
            json!({"n": 2})
        );
    }

    #[tokio::test]
    async fn delete_reports_whether_the_record_existed() {
        let store = InMemoryMemoryStore::new();
        let record = record("fact");
        assert!(!store.delete(&record.id, &cx()).await.unwrap());

        store.put(record.clone(), &cx()).await.unwrap();
        assert!(store.delete(&record.id, &cx()).await.unwrap());
        assert!(store.get(&record.id, &cx()).await.unwrap().is_none());
        assert!(!store.delete(&record.id, &cx()).await.unwrap());
    }

    #[tokio::test]
    async fn query_filters_by_kind() {
        let store = InMemoryMemoryStore::new();
        let fact = record("fact");
        let preference = record("preference");
        store.put(fact.clone(), &cx()).await.unwrap();
        store.put(preference.clone(), &cx()).await.unwrap();

        let query = MemoryQuery {
            kinds: vec![fact.kind.clone()],
            ..Default::default()
        };
        let matches = store.query(&query, &cx()).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].record.id, fact.id);
    }

    #[tokio::test]
    async fn semantic_fields_are_unsupported() {
        let store = InMemoryMemoryStore::new();
        let query = MemoryQuery {
            text: Some("anything".into()),
            ..Default::default()
        };
        let error = store.query(&query, &cx()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn an_expired_record_is_not_query_visible_but_is_still_addressable_by_id() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(2_000)));
        let store = InMemoryMemoryStore::new().with_clock(clock);
        let mut record = record("fact");
        record.expires_at = Some(Timestamp::from_millis(1_500));
        store.put(record.clone(), &cx()).await.unwrap();

        assert!(store.query(&MemoryQuery::default(), &cx()).await.unwrap().is_empty());
        assert_eq!(store.get(&record.id, &cx()).await.unwrap(), Some(record));
    }

    #[tokio::test]
    async fn sweep_removes_expired_records_only() {
        let store = InMemoryMemoryStore::new();
        let mut expired = record("fact");
        expired.expires_at = Some(Timestamp::from_millis(500));
        let mut alive = record("fact");
        alive.expires_at = Some(Timestamp::from_millis(5_000));
        let forever = record("fact");

        store.put(expired.clone(), &cx()).await.unwrap();
        store.put(alive.clone(), &cx()).await.unwrap();
        store.put(forever.clone(), &cx()).await.unwrap();

        let removed = store.sweep_expired(Timestamp::from_millis(1_000)).await.unwrap();
        assert_eq!(removed, 1);
        assert!(store.get(&expired.id, &cx()).await.unwrap().is_none());
        assert!(store.get(&alive.id, &cx()).await.unwrap().is_some());
        assert!(store.get(&forever.id, &cx()).await.unwrap().is_some());
    }
}
