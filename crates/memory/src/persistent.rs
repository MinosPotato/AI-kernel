//! [`RedbMemoryStore`]: the same [`MemoryStore`] contract, kept on disk.
//!
//! # What is stored, and where the truth lives
//!
//! Three tables in the shared [`Db`], all namespaced under `mem.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `mem.records` | id | owner, kind, content, metadata, timestamps, embedding |
//! | `mem.by_kind` | (kind, id) | nothing — the key is the index |
//! | `mem.by_expiry` | (expires at, id) | nothing — the key is the index |
//! | `mem.by_owner` | (owner, id) | nothing — the key is the index |
//!
//! A record's own id is the primary key of `mem.records`, so it is not repeated inside the
//! stored value: storing it twice would create a class of inconsistency — key and value
//! disagreeing about which record this is — that cannot arise if the value never says.
//! `mem.by_kind` and `mem.by_expiry` exist because [`MemoryStore::query`] needs to find
//! records by kind or reclaim them by expiry without scanning every record in the database;
//! all three indexes are kept exactly in step with `mem.records` by [`put_blocking`] and
//! [`delete_blocking`], in the same write transaction as the record itself.
//!
//! `mem.by_owner` is what keeps a query that names no kind from reading records it may not
//! return. Without it such a query would decode every record in the database and then discard
//! the ones belonging to other principals — correct, but it would have had another
//! principal's memories in this process's heap to do it. Scanning the owner's own range
//! instead means they are never read at all, which is a cheaper answer and a narrower one.
//!
//! # Atomicity
//!
//! Every mutation — a put, a delete, a sweep — is one redb write transaction covering the
//! record and both indexes together. A crash or a refused write between them is not a state
//! this store can be in: either the transaction committed and all three agree, or it did not
//! and none of them changed.
//!
//! # Blocking
//!
//! redb is synchronous. Each method moves its entire transaction onto a blocking thread with
//! [`spawn_blocking`](tokio::task::spawn_blocking) and awaits the result, so a transaction is
//! opened and committed within one closure and is never held across an await.
//!
//! Writers queue on [`Db::writes`] first — the database's queue, not this store's, because the
//! transcript store writes to the same file and a per-subsystem queue would order only half
//! the writers.
//!
//! # What `get` and `delete` do not check
//!
//! Neither applies the expiry filter [`MemoryStore::query`] does — see [`crate::expiry`] for
//! why addressing a record by id is exempt from the visibility rule that applies to
//! enumerating them.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryId, MemoryKind, MemoryMatch, MemoryQuery, MemoryRecord, MemoryStore};
use aik_api::permission::{Principal, PrincipalId};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, Result};
use aik_store::redb::{
    Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};
use aik_store::{Db, store_error};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::expiry::{DEFAULT_SWEEP_BATCH, ExpirySweeper, is_live};
use crate::owner::{authorize, principal_of};
use crate::query::{matches_metadata, rank, reject_unsupported, requested_kinds};

/// One row per record, keyed by id.
const RECORDS: TableDefinition<'static, u128, &[u8]> = TableDefinition::new("mem.records");

/// A record's kind, indexed for [`MemoryStore::query`]'s kind filter.
const BY_KIND: TableDefinition<'static, (&str, u128), ()> = TableDefinition::new("mem.by_kind");

/// A record's expiry, indexed so the sweep can find due records without a full scan.
const BY_EXPIRY: TableDefinition<'static, (u64, u128), ()> = TableDefinition::new("mem.by_expiry");

/// A record's owner, indexed so a query reads only what it may return.
const BY_OWNER: TableDefinition<'static, (&str, u128), ()> = TableDefinition::new("mem.by_owner");

/// A record, as stored: everything a [`MemoryRecord`] carries except its id, which is the
/// key it is stored under.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRecord {
    owner: PrincipalId,
    kind: MemoryKind,
    content: Value,
    #[serde(default)]
    metadata: Map<String, Value>,
    created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding: Option<Vec<f32>>,
}

impl StoredRecord {
    fn from_record(record: MemoryRecord) -> (MemoryId, Self) {
        (
            record.id,
            Self {
                owner: record.owner,
                kind: record.kind,
                content: record.content,
                metadata: record.metadata,
                created_at: record.created_at,
                expires_at: record.expires_at,
                embedding: record.embedding,
            },
        )
    }

    fn into_record(self, id: MemoryId) -> MemoryRecord {
        MemoryRecord {
            id,
            owner: self.owner,
            kind: self.kind,
            content: self.content,
            metadata: self.metadata,
            created_at: self.created_at,
            expires_at: self.expires_at,
            embedding: self.embedding,
        }
    }
}

/// A [`MemoryStore`] that keeps records in the kernel's shared [`Db`].
///
/// The persistent counterpart of [`InMemoryMemoryStore`](crate::InMemoryMemoryStore): same
/// contract, same guarantees, and one more — a restart does not lose what was remembered.
///
/// # Security
///
/// The database file is created `0600` inside a `0700` directory by [`Db::open`] — see
/// [`aik_store`] — for the same reason it matters for a context transcript: this is content a
/// model wrote or was told to remember, not something another local account should be able
/// to read.
pub struct RedbMemoryStore {
    db: Arc<Db>,
    clock: SharedClock,
    sweep_batch: usize,
}

impl std::fmt::Debug for RedbMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbMemoryStore")
            .field("path", &self.db.path())
            .field("sweep_batch", &self.sweep_batch)
            .finish()
    }
}

impl RedbMemoryStore {
    /// Opens the store over an already-open database.
    ///
    /// Creates the three tables if they are absent, so that later reads — which run in read
    /// transactions and cannot create anything — always find them.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        create_tables(db.database())?;
        Ok(Self {
            db,
            clock: Arc::new(SystemClock),
            sweep_batch: DEFAULT_SWEEP_BATCH,
        })
    }

    /// Overrides the clock used to decide which records are live. Defaults to the system
    /// clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Overrides how many records one sweep transaction reclaims, from
    /// [`DEFAULT_SWEEP_BATCH`].
    ///
    /// Lower it where a commit is expensive and the write slot is contended, so that
    /// housekeeping yields to real traffic more often. Raising it trades that back for fewer
    /// commits. It changes only how the work is divided: a sweep reclaims everything due
    /// either way.
    ///
    /// # Panics
    ///
    /// Panics if `records` is zero, which would make a sweep loop without ever reclaiming
    /// anything.
    #[must_use]
    pub fn with_sweep_batch(mut self, records: usize) -> Self {
        assert!(
            records > 0,
            "a sweep batch of zero would never make progress"
        );
        self.sweep_batch = records;
        self
    }

    /// The database this store writes to.
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }
}

#[async_trait]
impl MemoryStore for RedbMemoryStore {
    async fn put(&self, record: MemoryRecord, cx: &ExecutionContext) -> Result<()> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let _queued = self.db.writes().lock().await;
        let joined =
            tokio::task::spawn_blocking(move || put_blocking(db.database(), record, &principal))
                .await;
        finish(joined, "storing a memory record")
    }

    async fn get(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<Option<MemoryRecord>> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let id = *id;
        let joined =
            tokio::task::spawn_blocking(move || get_blocking(db.database(), id, &principal)).await;
        finish(joined, "reading a memory record")
    }

    async fn delete(&self, id: &MemoryId, cx: &ExecutionContext) -> Result<bool> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let id = *id;
        let _queued = self.db.writes().lock().await;
        let joined =
            tokio::task::spawn_blocking(move || delete_blocking(db.database(), id, &principal))
                .await;
        finish(joined, "deleting a memory record")
    }

    async fn query(&self, query: &MemoryQuery, cx: &ExecutionContext) -> Result<Vec<MemoryMatch>> {
        reject_unsupported(query)?;
        let db = self.db.clone();
        let principal = principal_of(cx);
        let query = query.clone();
        let now = self.clock.now();
        let joined = tokio::task::spawn_blocking(move || {
            query_blocking(db.database(), &query, now, &principal)
        })
        .await;
        finish(joined, "querying memory records")
    }
}

#[async_trait]
impl ExpirySweeper for RedbMemoryStore {
    /// Reclaims in batches of [`DEFAULT_SWEEP_BATCH`], looping until nothing is due.
    ///
    /// The write lock is taken and released per batch rather than held for the whole sweep,
    /// so a backlog cannot starve the conversation the user is having; and each batch is its
    /// own transaction, so dropping this future between batches leaves the store consistent
    /// with whatever it had already reclaimed. See [`ExpirySweeper`] for why both matter.
    async fn sweep_expired(&self, now: Timestamp) -> Result<usize> {
        let mut total = 0usize;
        loop {
            let db = self.db.clone();
            let batch = self.sweep_batch;
            let removed = {
                let _queued = self.db.writes().lock().await;
                let joined = tokio::task::spawn_blocking(move || {
                    sweep_batch_blocking(db.database(), now, batch)
                })
                .await;
                finish(joined, "sweeping expired memory records")?
            };
            total += removed;

            // A short batch means the range ran out, so there is nothing left due at `now`.
            // Anything that expires *after* this call started is the next sweep's business.
            if removed < batch {
                return Ok(total);
            }
        }
    }
}

/// Unwraps a `spawn_blocking` result, turning a lost task into an error rather than a panic.
fn finish<T>(
    joined: std::result::Result<Result<T>, tokio::task::JoinError>,
    what: &'static str,
) -> Result<T> {
    match joined {
        Ok(result) => result,
        Err(error) => Err(Error::wrap(
            format!("the task {what} did not complete"),
            error,
        )),
    }
}

/// Creates the store's tables if they do not exist yet.
fn create_tables(db: &Database) -> Result<()> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning the memory schema transaction", error))?;
    {
        transaction
            .open_table(RECORDS)
            .map_err(|error| store_error("creating the memory record table", error))?;
        transaction
            .open_table(BY_KIND)
            .map_err(|error| store_error("creating the memory kind index", error))?;
        transaction
            .open_table(BY_EXPIRY)
            .map_err(|error| store_error("creating the memory expiry index", error))?;
        transaction
            .open_table(BY_OWNER)
            .map_err(|error| store_error("creating the memory owner index", error))?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing the memory schema", error))
}

/// Upserts one record and keeps all three indexes in step, in a single transaction.
///
/// The stored owner comes from `principal` on a first insert and from the record already
/// there on a replacement — never from the `record` argument, and never re-derived from the
/// caller. A record that changed hands on being revised would mean an agent acting on behalf
/// of a user became the owner of that user's memory by editing it, which is the opposite of
/// what delegation is for.
fn put_blocking(db: &Database, record: MemoryRecord, principal: &Principal) -> Result<()> {
    let (id, mut stored) = StoredRecord::from_record(record);
    let key = id_key(id);

    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a memory write", error))?;
    {
        let mut records = open_write(&transaction, RECORDS, "record")?;
        let previous = records
            .get(key)
            .map_err(|error| store_error("reading a memory record", error))?
            .map(|value| decode::<StoredRecord>("memory record", value.value()))
            .transpose()?;

        // Refused before anything is written, so the transaction carries no trace of a
        // replacement that was not allowed.
        stored.owner = match &previous {
            Some(previous) => {
                authorize(&id, &previous.owner, principal)?;
                previous.owner.clone()
            }
            None => principal.id.clone(),
        };
        let encoded = encode("memory record", &stored)?;

        records
            .insert(key, encoded.as_slice())
            .map_err(|error| store_error("writing a memory record", error))?;
        drop(records);

        // Only a first insert touches the owner index: a replacement keeps the owner it
        // already had, so the entry it would write is the entry already there.
        if previous.is_none() {
            let mut by_owner = open_write(&transaction, BY_OWNER, "owner index")?;
            by_owner
                .insert((stored.owner.as_str(), key), ())
                .map_err(|error| store_error("updating the memory owner index", error))?;
        }

        let mut by_kind = open_write(&transaction, BY_KIND, "kind index")?;
        if let Some(previous) = &previous {
            by_kind
                .remove((previous.kind.as_str(), key))
                .map_err(|error| store_error("updating the memory kind index", error))?;
        }
        by_kind
            .insert((stored.kind.as_str(), key), ())
            .map_err(|error| store_error("updating the memory kind index", error))?;
        drop(by_kind);

        let mut by_expiry = open_write(&transaction, BY_EXPIRY, "expiry index")?;
        if let Some(previous_expiry) = previous.and_then(|previous| previous.expires_at) {
            by_expiry
                .remove((previous_expiry.as_millis(), key))
                .map_err(|error| store_error("updating the memory expiry index", error))?;
        }
        if let Some(expires_at) = stored.expires_at {
            by_expiry
                .insert((expires_at.as_millis(), key), ())
                .map_err(|error| store_error("updating the memory expiry index", error))?;
        }
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing a memory write", error))
}

/// Reads one record by id, if it belongs to `principal`. Does not apply the expiry filter —
/// see the module documentation.
fn get_blocking(
    db: &Database,
    id: MemoryId,
    principal: &Principal,
) -> Result<Option<MemoryRecord>> {
    let key = id_key(id);
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a memory read", error))?;
    let records = open_read(&transaction, RECORDS, "record")?;
    let Some(value) = records
        .get(key)
        .map_err(|error| store_error("reading a memory record", error))?
    else {
        return Ok(None);
    };
    let stored: StoredRecord = decode("memory record", value.value())?;
    authorize(&id, &stored.owner, principal)?;
    Ok(Some(stored.into_record(id)))
}

/// Removes one record and all three of its index entries, in a single transaction. Does not
/// apply the expiry filter — a record can still be deleted by id after it has expired but
/// before the sweep reaches it.
fn delete_blocking(db: &Database, id: MemoryId, principal: &Principal) -> Result<bool> {
    let key = id_key(id);
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a memory delete", error))?;
    let removed = {
        let mut records = open_write(&transaction, RECORDS, "record")?;

        // Read before removing, so that a refusal leaves the row where it was. Removing
        // first and putting it back on refusal would be the same thing only if nothing ever
        // went wrong in between.
        let stored: StoredRecord = {
            let Some(value) = records
                .get(key)
                .map_err(|error| store_error("reading a memory record", error))?
            else {
                // Nothing to do, and nothing written: the transaction is dropped, not
                // committed.
                return Ok(false);
            };
            decode("memory record", value.value())?
        };
        authorize(&id, &stored.owner, principal)?;
        records
            .remove(key)
            .map_err(|error| store_error("removing a memory record", error))?;
        drop(records);

        let mut by_kind = open_write(&transaction, BY_KIND, "kind index")?;
        by_kind
            .remove((stored.kind.as_str(), key))
            .map_err(|error| store_error("removing a memory kind index entry", error))?;
        drop(by_kind);

        let mut by_owner = open_write(&transaction, BY_OWNER, "owner index")?;
        by_owner
            .remove((stored.owner.as_str(), key))
            .map_err(|error| store_error("removing a memory owner index entry", error))?;
        drop(by_owner);

        if let Some(expires_at) = stored.expires_at {
            let mut by_expiry = open_write(&transaction, BY_EXPIRY, "expiry index")?;
            by_expiry
                .remove((expires_at.as_millis(), key))
                .map_err(|error| store_error("removing a memory expiry index entry", error))?;
        }
        true
    };
    transaction
        .commit()
        .map_err(|error| store_error("committing a memory delete", error))?;
    Ok(removed)
}

/// Answers a query: candidates are gathered from the owner index (a query naming no kind) or
/// from the kind index (one that does), then filtered for ownership, liveness and metadata,
/// ranked and limited by [`crate::query`].
///
/// Neither path scans `mem.records` whole. A query naming no kind reads only the ranges
/// belonging to principals the caller may act for, so another principal's records are never
/// decoded rather than being decoded and then discarded; a query naming kinds reads those
/// kinds and filters, because a record's kind is the narrower of the two and a kind's range
/// is bounded by how many records carry it.
fn query_blocking(
    db: &Database,
    query: &MemoryQuery,
    now: Timestamp,
    principal: &Principal,
) -> Result<Vec<MemoryMatch>> {
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a memory query", error))?;
    let records = open_read(&transaction, RECORDS, "record")?;

    let kinds = requested_kinds(query);
    let candidates: Vec<MemoryRecord> = if kinds.is_empty() {
        let by_owner = open_read(&transaction, BY_OWNER, "owner index")?;
        let mut candidates = Vec::new();
        for owner in accessible_owners(principal) {
            for row in by_owner
                .range(owner_range(&owner))
                .map_err(|error| store_error("scanning the memory owner index", error))?
            {
                let (key, _) =
                    row.map_err(|error| store_error("reading the memory owner index", error))?;
                let (_, record_key) = key.value();
                let id = id_from_key(record_key);
                let value = records
                    .get(record_key)
                    .map_err(|error| store_error("reading a memory record", error))?
                    .ok_or_else(|| {
                        Error::other(format!(
                            "the memory owner index names record `{id}` under owner `{owner}`, \
                             but no record is stored under that id"
                        ))
                    })?;
                let stored: StoredRecord = decode("memory record", value.value())?;
                candidates.push(stored.into_record(id));
            }
        }
        candidates
    } else {
        let by_kind = open_read(&transaction, BY_KIND, "kind index")?;
        let mut candidates = Vec::new();
        for kind in &kinds {
            for row in by_kind
                .range(kind_range(kind))
                .map_err(|error| store_error("scanning the memory kind index", error))?
            {
                let (key, _) =
                    row.map_err(|error| store_error("reading the memory kind index", error))?;
                let (_, record_key) = key.value();
                let id = id_from_key(record_key);
                let value = records
                    .get(record_key)
                    .map_err(|error| store_error("reading a memory record", error))?
                    .ok_or_else(|| {
                        Error::other(format!(
                            "the memory kind index names record `{id}` under kind `{kind}`, but no \
                             record is stored under that id"
                        ))
                    })?;
                let stored: StoredRecord = decode("memory record", value.value())?;
                candidates.push(stored.into_record(id));
            }
        }
        candidates
    };

    let candidates: Vec<MemoryRecord> = candidates
        .into_iter()
        // Applied on both paths, not only the kind one: the owner index narrows the read, it
        // does not define the rule. Filtering here is what makes the rule the same rule the
        // in-memory store applies, and what keeps a stale index entry from widening it.
        .filter(|record| principal.may_act_for(&record.owner))
        .filter(|record| is_live(record.expires_at, now))
        .filter(|record| matches_metadata(record, &query.metadata))
        .collect();

    Ok(rank(candidates, query.limit))
}

/// The owners `principal` may read records of: itself, and whoever it acts on behalf of.
///
/// At most two, deduplicated — a principal that names itself as its own delegator must not
/// make every record come back twice.
fn accessible_owners(principal: &Principal) -> Vec<PrincipalId> {
    let mut owners = vec![principal.id.clone()];
    if let Some(delegator) = &principal.on_behalf_of
        && delegator != &principal.id
    {
        owners.push(delegator.clone());
    }
    owners
}

/// Removes up to `limit` records whose expiry is at or before `now`, along with both index
/// entries, in a single transaction: either the whole batch commits, or none of it does.
///
/// The caller loops until a batch comes back short — see
/// [`ExpirySweeper::sweep_expired`](crate::ExpirySweeper::sweep_expired). Reclaiming
/// everything in one transaction would be simpler and is deliberately not done: the size of a
/// sweep is decided by how much expired while nobody was looking, and an unbounded
/// transaction is an unbounded allocation, an unbounded hold on the write slot, and an
/// operation that cannot be abandoned when the kernel is asked to stop.
///
/// Each batch restarts the range scan from the beginning of the index. That is a B-tree seek
/// to the leftmost remaining key, not a rescan: the entries the previous batch removed are no
/// longer there to be skipped.
///
/// An expiry entry naming a record that `mem.records` no longer has is reported as an error
/// rather than silently dropped, the same way `query_blocking` treats a dangling kind-index
/// entry: as corruption, not as an empty result.
///
/// A sweep is not scoped to a principal and deliberately takes no `Principal`. It reclaims
/// whatever has expired, whoever owns it: retention is a property of the record, and
/// housekeeping that could only reclaim the records of whichever principal happened to
/// trigger it would leave everyone else's expired data on disk for ever.
fn sweep_batch_blocking(db: &Database, now: Timestamp, limit: usize) -> Result<usize> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a memory sweep", error))?;

    let due: Vec<(u64, u128)> = {
        let by_expiry = open_write(&transaction, BY_EXPIRY, "expiry index")?;
        let mut due = Vec::new();
        for row in by_expiry
            .range(..=(now.as_millis(), u128::MAX))
            .map_err(|error| store_error("scanning the memory expiry index", error))?
        {
            let (key, _) =
                row.map_err(|error| store_error("reading the memory expiry index", error))?;
            due.push(key.value());
            if due.len() >= limit {
                break;
            }
        }
        due
    };

    if due.is_empty() {
        // Nothing to do, and nothing written: the transaction is dropped rather than
        // committed, so an idle sweep costs no fsync.
        return Ok(0);
    }

    let mut removed = 0usize;
    {
        let mut records = open_write(&transaction, RECORDS, "record")?;
        let mut by_kind = open_write(&transaction, BY_KIND, "kind index")?;
        let mut by_owner = open_write(&transaction, BY_OWNER, "owner index")?;
        let mut by_expiry = open_write(&transaction, BY_EXPIRY, "expiry index")?;

        for (expires_at_ms, record_key) in due {
            let id = id_from_key(record_key);
            let value = records
                .remove(record_key)
                .map_err(|error| store_error("removing an expired memory record", error))?
                .ok_or_else(|| {
                    Error::other(format!(
                        "the memory expiry index names record `{id}` at {expires_at_ms}ms, but no \
                         record is stored under that id"
                    ))
                })?;
            let stored: StoredRecord = decode("memory record", value.value())?;
            by_kind
                .remove((stored.kind.as_str(), record_key))
                .map_err(|error| store_error("removing a memory kind index entry", error))?;
            by_owner
                .remove((stored.owner.as_str(), record_key))
                .map_err(|error| store_error("removing a memory owner index entry", error))?;
            by_expiry
                .remove((expires_at_ms, record_key))
                .map_err(|error| store_error("removing a memory expiry index entry", error))?;
            removed += 1;
        }
    }

    transaction
        .commit()
        .map_err(|error| store_error("committing a memory sweep", error))?;
    Ok(removed)
}

/// Opens a table for writing, naming it in any failure.
fn open_write<'t, K, V>(
    transaction: &'t WriteTransaction,
    table: TableDefinition<'static, K, V>,
    what: &str,
) -> Result<aik_store::redb::Table<'t, K, V>>
where
    K: aik_store::redb::Key + 'static,
    V: aik_store::redb::Value + 'static,
{
    transaction
        .open_table(table)
        .map_err(|error| store_error(&format!("opening the memory {what} table"), error))
}

/// Opens a table for reading, naming it in any failure.
fn open_read<K, V>(
    transaction: &aik_store::redb::ReadTransaction,
    table: TableDefinition<'static, K, V>,
    what: &str,
) -> Result<aik_store::redb::ReadOnlyTable<K, V>>
where
    K: aik_store::redb::Key + 'static,
    V: aik_store::redb::Value + 'static,
{
    transaction
        .open_table(table)
        .map_err(|error| store_error(&format!("opening the memory {what} table"), error))
}

/// The key a record is stored under.
fn id_key(id: MemoryId) -> u128 {
    id.as_uuid().as_u128()
}

/// Reconstructs an id from the raw key it was found under.
fn id_from_key(key: u128) -> MemoryId {
    MemoryId::from_uuid(uuid::Uuid::from_u128(key))
}

/// Every `mem.by_kind` entry for one kind, and nothing else.
fn kind_range(kind: &MemoryKind) -> std::ops::RangeInclusive<(&str, u128)> {
    (kind.as_str(), 0)..=(kind.as_str(), u128::MAX)
}

/// Every `mem.by_owner` entry for one owner, and nothing else.
fn owner_range(owner: &PrincipalId) -> std::ops::RangeInclusive<(&str, u128)> {
    (owner.as_str(), 0)..=(owner.as_str(), u128::MAX)
}

/// Encodes a stored value, attributing a failure to what was being written.
fn encode<T: Serialize>(what: &'static str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| Error::wrap(format!("encoding the {what}"), error))
}

/// Decodes a stored value.
///
/// A failure here means the bytes on disk are not what this build writes. That is corrupt
/// or foreign data, not a missing value, so it is an error rather than a `None`: silently
/// treating an unreadable record as absent would let a caller act as though it had been
/// forgotten when it was in fact merely unreadable.
fn decode<T: for<'de> Deserialize<'de>>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| Error::wrap(format!("decoding the {what}"), error))
}
