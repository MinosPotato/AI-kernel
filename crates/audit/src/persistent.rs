//! [`RedbAuditStore`]: the same [`AuditStore`] contract, kept on disk.
//!
//! # What is stored, and where the truth lives
//!
//! Five tables in the shared [`Db`], all namespaced under `audit.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `audit.records` | sequence | the [`AuditEntry`], verbatim |
//! | `audit.by_time` | (timestamp, sequence) | nothing — the key is the index |
//! | `audit.by_principal` | (principal, sequence) | nothing — the key is the index |
//! | `audit.by_correlation` | (correlation, sequence) | nothing — the key is the index |
//! | `audit.meta` | `next_sequence` | the highest sequence number ever issued |
//!
//! A record's sequence number is the primary key of `audit.records`, so it is not repeated
//! inside the stored value: a key and a value that can disagree about which record this is
//! are a class of inconsistency that cannot arise if the value never says. The three indexes
//! exist because the three questions an operator actually asks — "what did this principal
//! do", "what happened in this one operation", "what happened between these times" — must not
//! require decoding the whole trail, which is the largest collection this system keeps.
//!
//! `audit.by_principal` carries **two** entries for a delegated action: one under the actor
//! and one under whoever it acted for. That is what makes "show me what was done on my
//! behalf" a range scan rather than a full scan, and it is the same relation
//! [`AuditRecord::visible_to`] enforces, so the index cannot drift from the rule.
//!
//! A record about the trail itself — a gap, a retention marker — is indexed under
//! [`Principal::SYSTEM`], and a principal-filtered query scans that range too. Without the
//! second scan a filtered view could omit the one record that says the view is incomplete.
//!
//! # Sequence numbers
//!
//! Assigned inside the same write transaction that stores the record, as one past the counter
//! in `audit.meta`. The counter — rather than the largest key in `audit.records` — is what
//! makes a number never reused: a sweep that removed every record in the table would leave no
//! largest key to count from, and numbering would restart at 1, silently giving two different
//! records the same identity in an exported trail. The counter lives in the database rather
//! than in this process, so a restart does not reset it either.
//!
//! # Atomicity
//!
//! Every mutation — an append, a sweep batch — is one redb write transaction covering the
//! record and all three of its index entries together. A crash between them is not a state
//! this store can be in.
//!
//! A sweep batch writes its own [`RetentionApplied`] marker *in the batch's transaction*
//! rather than one marker at the end of the whole sweep. A crash mid-sweep therefore leaves a
//! trail whose markers account exactly for what was removed, rather than one that lost
//! records with nothing to say so.
//!
//! # Blocking
//!
//! redb is synchronous. Each method moves its entire transaction onto a blocking thread with
//! [`spawn_blocking`](tokio::task::spawn_blocking) and awaits the result, so a transaction is
//! opened and committed within one closure and is never held across an await. Writers queue
//! on [`Db::writes`] first — the database's queue, not this store's, because the transcript,
//! memory and scheduler stores write to the same file.

use std::sync::Arc;

use aik_api::audit::{AuditEntry, AuditQuery, AuditRecord, AuditStore, RetentionApplied};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, Result};
use aik_store::redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable, Table,
    TableDefinition, WriteTransaction,
};
use aik_store::{Db, store_error};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::retention::{AuditRetentionSweeper, DEFAULT_RETENTION_BATCH, survives_retention};

/// One row per record, keyed by its sequence number.
const RECORDS: TableDefinition<'static, u64, &[u8]> = TableDefinition::new("audit.records");

/// A record's timestamp, indexed for range queries and for the retention sweep.
const BY_TIME: TableDefinition<'static, (u64, u64), ()> = TableDefinition::new("audit.by_time");

/// Every principal a record concerns — the actor, and whoever it acted for.
const BY_PRINCIPAL: TableDefinition<'static, (&str, u64), ()> =
    TableDefinition::new("audit.by_principal");

/// A record's operation, indexed so a decision can be joined to the invocation it gated.
const BY_CORRELATION: TableDefinition<'static, (u128, u64), ()> =
    TableDefinition::new("audit.by_correlation");

/// The store's own bookkeeping, keyed by a short ASCII name.
const META: TableDefinition<'static, &str, u64> = TableDefinition::new("audit.meta");

/// The key the sequence counter is stored under in [`META`].
const NEXT_SEQUENCE_KEY: &str = "next_sequence";

/// The correlation every record without one is indexed under.
///
/// A gap and a retention marker belong to no operation. Indexing them under zero rather than
/// leaving them out keeps `audit.by_correlation` a complete mirror of `audit.records`, which
/// is what lets a consistency check compare the two by count.
const NO_CORRELATION: u128 = 0;

/// An [`AuditStore`] that keeps the trail in the kernel's shared [`Db`].
///
/// # Security
///
/// The database file is created `0600` inside a `0700` directory by [`Db::open`] — see
/// [`aik_store`]. For an audit trail that matters twice over: it records which principals
/// were allowed to touch which resources, which is a map of this system's authority, and it
/// records refusals, which is a map of where somebody has already tried.
pub struct RedbAuditStore {
    db: Arc<Db>,
    clock: SharedClock,
    sweep_batch: usize,
}

impl std::fmt::Debug for RedbAuditStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbAuditStore")
            .field("path", &self.db.path())
            .field("sweep_batch", &self.sweep_batch)
            .finish()
    }
}

impl RedbAuditStore {
    /// Opens the store over an already-open database.
    ///
    /// Creates the five tables if they are absent, so that later reads — which run in read
    /// transactions and cannot create anything — always find them.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        create_tables(db.database())?;
        Ok(Self {
            db,
            clock: Arc::new(SystemClock),
            sweep_batch: DEFAULT_RETENTION_BATCH,
        })
    }

    /// Overrides the clock a retention marker is stamped with. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Overrides how many records one sweep transaction reclaims, from
    /// [`DEFAULT_RETENTION_BATCH`].
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
impl AuditStore for RedbAuditStore {
    async fn append(&self, entry: AuditEntry) -> Result<u64> {
        let db = self.db.clone();
        let _queued = self.db.writes().lock().await;
        let joined = tokio::task::spawn_blocking(move || {
            let transaction = db
                .database()
                .begin_write()
                .map_err(|error| store_error("beginning an audit append", error))?;
            let sequence = append_blocking(&transaction, entry)?;
            transaction
                .commit()
                .map_err(|error| store_error("committing an audit append", error))?;
            Ok(sequence)
        })
        .await;
        finish(joined, "appending an audit record")
    }

    async fn query(&self, query: &AuditQuery, cx: &ExecutionContext) -> Result<Vec<AuditRecord>> {
        let db = self.db.clone();
        let reader = cx.principal_or_system();
        let query = query.clone();
        let joined =
            tokio::task::spawn_blocking(move || query_blocking(db.database(), &query, &reader))
                .await;
        finish(joined, "querying the audit trail")
    }

    async fn last_sequence(&self) -> Result<u64> {
        let db = self.db.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let transaction = db
                .database()
                .begin_read()
                .map_err(|error| store_error("beginning an audit read", error))?;
            let meta = open_read(&transaction, META, "sequence")?;
            Ok(meta
                .get(NEXT_SEQUENCE_KEY)
                .map_err(|error| store_error("reading the audit sequence counter", error))?
                .map_or(0, |value| value.value()))
        })
        .await;
        finish(joined, "reading the audit trail's length")
    }
}

#[async_trait]
impl AuditRetentionSweeper for RedbAuditStore {
    /// Reclaims in batches of [`RedbAuditStore::with_sweep_batch`], looping until nothing at
    /// or before `cutoff` is left.
    ///
    /// The write lock is taken and released per batch rather than held for the whole sweep,
    /// so a backlog cannot starve the conversation somebody is having; and each batch is its
    /// own transaction, so dropping this future between batches leaves the store consistent
    /// with whatever it had already reclaimed.
    async fn sweep_older_than(&self, cutoff: Timestamp) -> Result<usize> {
        let mut total = 0usize;
        loop {
            let db = self.db.clone();
            let batch = self.sweep_batch;
            let now = self.clock.now();
            let removed = {
                let _queued = self.db.writes().lock().await;
                let joined = tokio::task::spawn_blocking(move || {
                    sweep_batch_blocking(db.database(), cutoff, batch, now)
                })
                .await;
                finish(joined, "sweeping the audit trail")?
            };
            total += removed;

            // A short batch means the range ran out, so nothing at or before `cutoff` is
            // left. Anything written after this call started is a later sweep's business.
            if removed < batch {
                return Ok(total);
            }
        }
    }

    async fn count_older_than(&self, cutoff: Timestamp) -> Result<usize> {
        let db = self.db.clone();
        let joined =
            tokio::task::spawn_blocking(move || count_due_blocking(db.database(), cutoff)).await;
        finish(joined, "counting the audit trail's stale records")
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
        .map_err(|error| store_error("beginning the audit schema transaction", error))?;
    {
        open_write(&transaction, RECORDS, "record")?;
        open_write(&transaction, BY_TIME, "time index")?;
        open_write(&transaction, BY_PRINCIPAL, "principal index")?;
        open_write(&transaction, BY_CORRELATION, "correlation index")?;
        open_write(&transaction, META, "sequence")?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing the audit schema", error))
}

/// Stores one entry and all of its index entries, inside the caller's transaction.
///
/// Takes the transaction rather than opening one so that a retention batch can append its own
/// marker in the same transaction that removed the records the marker accounts for.
fn append_blocking(transaction: &WriteTransaction, entry: AuditEntry) -> Result<u64> {
    let sequence = issued_sequence(transaction)?
        .checked_add(1)
        .ok_or_else(|| Error::other("the audit trail has run out of sequence numbers"))?;

    let encoded = encode("audit record", &entry)?;
    let timestamp = entry.timestamp().as_millis();
    let correlation = entry.correlation().map_or(NO_CORRELATION, |correlation| {
        correlation.as_uuid().as_u128()
    });
    let principals = indexed_principals(&entry);

    {
        let mut records = open_write(transaction, RECORDS, "record")?;
        records
            .insert(sequence, encoded.as_slice())
            .map_err(|error| store_error("writing an audit record", error))?;
    }
    {
        let mut by_time = open_write(transaction, BY_TIME, "time index")?;
        by_time
            .insert((timestamp, sequence), ())
            .map_err(|error| store_error("updating the audit time index", error))?;
    }
    {
        let mut by_principal = open_write(transaction, BY_PRINCIPAL, "principal index")?;
        for principal in &principals {
            by_principal
                .insert((principal.as_str(), sequence), ())
                .map_err(|error| store_error("updating the audit principal index", error))?;
        }
    }
    {
        let mut by_correlation = open_write(transaction, BY_CORRELATION, "correlation index")?;
        by_correlation
            .insert((correlation, sequence), ())
            .map_err(|error| store_error("updating the audit correlation index", error))?;
    }
    {
        // Last, and in the same transaction: a crash before the commit leaves the counter
        // where it was, so the number this append would have used is simply issued again to
        // a record that actually exists.
        let mut meta = open_write(transaction, META, "sequence")?;
        meta.insert(NEXT_SEQUENCE_KEY, sequence)
            .map_err(|error| store_error("recording the audit sequence counter", error))?;
    }

    Ok(sequence)
}

/// Every principal a record is indexed under: the actor, and whoever it acted for.
///
/// Deduplicated — an actor that names itself as its own delegator must not make every query
/// return the record twice.
fn indexed_principals(entry: &AuditEntry) -> Vec<PrincipalId> {
    let actor = entry.principal();
    let mut principals = vec![actor.clone()];
    if let Some(delegator) = entry.on_behalf_of()
        && delegator != &actor
    {
        principals.push(delegator.clone());
    }
    principals
}

/// The highest sequence number ever issued, inside a write transaction.
fn issued_sequence(transaction: &WriteTransaction) -> Result<u64> {
    let meta = open_write(transaction, META, "sequence")?;
    Ok(meta
        .get(NEXT_SEQUENCE_KEY)
        .map_err(|error| store_error("reading the audit sequence counter", error))?
        .map_or(0, |value| value.value()))
}

/// Answers a query, reading only the index the query's narrowest filter points at.
///
/// The chosen index decides only *what is read*, never what is returned: visibility and every
/// filter are applied to each candidate afterwards, so a wrong choice here could cost work but
/// could not widen an answer.
fn query_blocking(
    db: &Database,
    query: &AuditQuery,
    reader: &Principal,
) -> Result<Vec<AuditRecord>> {
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning an audit query", error))?;
    let records = open_read(&transaction, RECORDS, "record")?;
    let limit = query.limit.unwrap_or(usize::MAX);

    let mut sequences: Vec<u64> = if let Some(correlation) = &query.correlation {
        let by_correlation = open_read(&transaction, BY_CORRELATION, "correlation index")?;
        let key = correlation.as_uuid().as_u128();
        collect_second(
            by_correlation
                .range((key, 0)..=(key, u64::MAX))
                .map_err(|error| store_error("scanning the audit correlation index", error))?,
            "the audit correlation index",
            &records,
            query,
            reader,
            limit,
        )?
    } else if let Some(principal) = &query.principal {
        let by_principal = open_read(&transaction, BY_PRINCIPAL, "principal index")?;
        let mut sequences = Vec::new();
        // The named principal's own range, then the system's — the second is what keeps a
        // gap or a retention marker visible in a filtered view. Both are capped at `limit`
        // independently, which is sufficient: the merged top `limit` can draw at most
        // `limit` records from either.
        for name in scanned_principals(principal) {
            let range = by_principal
                .range((name.as_str(), 0)..=(name.as_str(), u64::MAX))
                .map_err(|error| store_error("scanning the audit principal index", error))?;
            sequences.extend(collect_second(
                range,
                "the audit principal index",
                &records,
                query,
                reader,
                limit,
            )?);
        }
        sequences
    } else {
        let by_time = open_read(&transaction, BY_TIME, "time index")?;
        let since = query.since.map_or(0, Timestamp::as_millis);
        let until = query.until.map_or(u64::MAX, Timestamp::as_millis);
        collect_second(
            by_time
                .range((since, 0)..=(until, u64::MAX))
                .map_err(|error| store_error("scanning the audit time index", error))?,
            "the audit time index",
            &records,
            query,
            reader,
            limit,
        )?
    };

    // Newest first, by sequence: the order the trail was written in, which is the order an
    // operator reads it in. Sorting rather than trusting the scan order is what makes the two
    // multi-range paths — and a clock that stepped backwards — produce one defined answer.
    sequences.sort_unstable_by(|left, right| right.cmp(left));
    sequences.dedup();
    sequences.truncate(limit);

    sequences
        .into_iter()
        .map(|sequence| read_record(&records, sequence, "a query"))
        .collect()
}

/// The principal ranges a principal-filtered query reads.
///
/// The named one, plus the system's unless that is the named one already: records about the
/// trail are attributed to the system, and no filter may hide them.
fn scanned_principals(principal: &PrincipalId) -> Vec<PrincipalId> {
    let system = PrincipalId::new(Principal::SYSTEM);
    if principal == &system {
        vec![system]
    } else {
        vec![principal.clone(), system]
    }
}

/// Walks an index range newest-first, keeping the sequence numbers whose records the reader
/// may see and the query matches, and stopping at `limit`.
fn collect_second<'a, K, I>(
    range: I,
    index: &str,
    records: &ReadOnlyTable<u64, &'static [u8]>,
    query: &AuditQuery,
    reader: &Principal,
    limit: usize,
) -> Result<Vec<u64>>
where
    I: DoubleEndedIterator<
        Item = std::result::Result<
            (K, aik_store::redb::AccessGuard<'a, ()>),
            aik_store::redb::StorageError,
        >,
    >,
    K: SecondOfKey,
{
    let mut kept = Vec::new();
    for row in range.rev() {
        let (key, _) = row.map_err(|error| store_error(&format!("reading {index}"), error))?;
        let sequence = key.sequence();
        let record = read_record(records, sequence, index)?;
        if record.visible_to(reader) && query.matches(&record) {
            kept.push(sequence);
            if kept.len() >= limit {
                break;
            }
        }
    }
    Ok(kept)
}

/// An index key whose second half is a sequence number.
///
/// The three indexes differ only in the type of their first half, so this is what lets one
/// scan serve all of them without a macro or three near-identical loops.
trait SecondOfKey {
    /// The sequence number this key points at.
    fn sequence(&self) -> u64;
}

impl SecondOfKey for aik_store::redb::AccessGuard<'_, (u64, u64)> {
    fn sequence(&self) -> u64 {
        self.value().1
    }
}

impl SecondOfKey for aik_store::redb::AccessGuard<'_, (u128, u64)> {
    fn sequence(&self) -> u64 {
        self.value().1
    }
}

impl SecondOfKey for aik_store::redb::AccessGuard<'_, (&str, u64)> {
    fn sequence(&self) -> u64 {
        self.value().1
    }
}

/// Reads one record by sequence number.
///
/// An index entry naming a record that `audit.records` does not hold is corruption, and is
/// reported as such rather than skipped: an audit store that quietly dropped what it could
/// not find would be indistinguishable from one with nothing to show.
fn read_record<T: ReadableTable<u64, &'static [u8]>>(
    records: &T,
    sequence: u64,
    index: &str,
) -> Result<AuditRecord> {
    let value = records
        .get(sequence)
        .map_err(|error| store_error("reading an audit record", error))?
        .ok_or_else(|| {
            Error::other(format!(
                "{index} names audit record {sequence}, but no record is stored under that \
                 sequence number"
            ))
        })?;
    let entry: AuditEntry = decode("audit record", value.value())?;
    Ok(AuditRecord { sequence, entry })
}

/// Removes up to `limit` sweepable records at or before `cutoff`, together with every index
/// entry naming them and a marker accounting for the batch, in one transaction.
///
/// The caller loops until a batch comes back short. Reclaiming everything in one transaction
/// would be simpler and is deliberately not done: the size of a sweep is decided by how much
/// accumulated while nobody was looking, and an unbounded transaction is an unbounded
/// allocation, an unbounded hold on the write slot, and an operation that cannot be abandoned
/// when the kernel is asked to stop.
fn sweep_batch_blocking(
    db: &Database,
    cutoff: Timestamp,
    limit: usize,
    now: Timestamp,
) -> Result<usize> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning an audit sweep", error))?;

    let due: Vec<(u64, u64)> = {
        let records = open_write(&transaction, RECORDS, "record")?;
        let by_time = open_write(&transaction, BY_TIME, "time index")?;
        let mut due = Vec::new();
        for row in by_time
            .range(..=(cutoff.as_millis(), u64::MAX))
            .map_err(|error| store_error("scanning the audit time index", error))?
        {
            let (key, _) =
                row.map_err(|error| store_error("reading the audit time index", error))?;
            let (timestamp, sequence) = key.value();
            let record = read_record(&records, sequence, "the audit time index")?;
            // Checked rather than assumed from the index: a gap and a retention marker are
            // in the time index like everything else, and they are exactly what a sweep must
            // leave behind.
            if survives_retention(&record, cutoff) {
                continue;
            }
            due.push((timestamp, sequence));
            if due.len() >= limit {
                break;
            }
        }
        due
    };

    if due.is_empty() {
        // Nothing to do and nothing written: the transaction is dropped rather than
        // committed, so an idle sweep costs no fsync and writes no marker.
        return Ok(0);
    }

    let removed = due.len();
    {
        let mut records = open_write(&transaction, RECORDS, "record")?;
        let mut by_time = open_write(&transaction, BY_TIME, "time index")?;
        let mut by_principal = open_write(&transaction, BY_PRINCIPAL, "principal index")?;
        let mut by_correlation = open_write(&transaction, BY_CORRELATION, "correlation index")?;

        for (timestamp, sequence) in due {
            let value = records
                .remove(sequence)
                .map_err(|error| store_error("removing an audit record", error))?
                .ok_or_else(|| {
                    Error::other(format!(
                        "the audit time index names record {sequence} at {timestamp}ms, but no \
                         record is stored under that sequence number"
                    ))
                })?;
            let entry: AuditEntry = decode("audit record", value.value())?;
            by_time
                .remove((timestamp, sequence))
                .map_err(|error| store_error("removing an audit time index entry", error))?;
            for principal in indexed_principals(&entry) {
                by_principal
                    .remove((principal.as_str(), sequence))
                    .map_err(|error| {
                        store_error("removing an audit principal index entry", error)
                    })?;
            }
            let correlation = entry.correlation().map_or(NO_CORRELATION, |correlation| {
                correlation.as_uuid().as_u128()
            });
            by_correlation
                .remove((correlation, sequence))
                .map_err(|error| store_error("removing an audit correlation index entry", error))?;
        }
    }

    // In the batch's own transaction: a crash mid-sweep leaves a trail whose markers account
    // exactly for what it lost, rather than one that lost records with nothing to say so.
    append_blocking(
        &transaction,
        AuditEntry::Retention(RetentionApplied {
            timestamp: now,
            cutoff,
            removed: removed as u64,
        }),
    )?;

    transaction
        .commit()
        .map_err(|error| store_error("committing an audit sweep", error))?;
    Ok(removed)
}

/// Counts the records a sweep at `cutoff` would remove, in one read transaction.
///
/// Reads the whole due range rather than stopping at a batch: a preview whose number was
/// capped at the batch size would understate what the sweep is about to do, which is the one
/// way a preview can be worse than no preview.
fn count_due_blocking(db: &Database, cutoff: Timestamp) -> Result<usize> {
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning an audit count", error))?;
    let records = open_read(&transaction, RECORDS, "record")?;
    let by_time = open_read(&transaction, BY_TIME, "time index")?;

    let mut due = 0usize;
    for row in by_time
        .range(..=(cutoff.as_millis(), u64::MAX))
        .map_err(|error| store_error("scanning the audit time index", error))?
    {
        let (key, _) = row.map_err(|error| store_error("reading the audit time index", error))?;
        let (_, sequence) = key.value();
        let record = read_record(&records, sequence, "the audit time index")?;
        if !survives_retention(&record, cutoff) {
            due += 1;
        }
    }
    Ok(due)
}

/// Opens a table for writing, naming it in any failure.
fn open_write<'t, K, V>(
    transaction: &'t WriteTransaction,
    table: TableDefinition<'static, K, V>,
    what: &str,
) -> Result<Table<'t, K, V>>
where
    K: aik_store::redb::Key + 'static,
    V: aik_store::redb::Value + 'static,
{
    transaction
        .open_table(table)
        .map_err(|error| store_error(&format!("opening the audit {what} table"), error))
}

/// Opens a table for reading, naming it in any failure.
fn open_read<K, V>(
    transaction: &ReadTransaction,
    table: TableDefinition<'static, K, V>,
    what: &str,
) -> Result<ReadOnlyTable<K, V>>
where
    K: aik_store::redb::Key + 'static,
    V: aik_store::redb::Value + 'static,
{
    transaction
        .open_table(table)
        .map_err(|error| store_error(&format!("opening the audit {what} table"), error))
}

/// Encodes a stored value, attributing a failure to what was being written.
fn encode<T: Serialize>(what: &'static str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| Error::wrap(format!("encoding the {what}"), error))
}

/// Decodes a stored value.
///
/// A failure here means the bytes on disk are not what this build writes: corrupt or foreign
/// data, not a missing value. Reported rather than skipped, because an audit trail that
/// silently omits what it cannot read is worse than one that refuses to be read.
fn decode<T: for<'de> Deserialize<'de>>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| Error::wrap(format!("decoding the {what}"), error))
}
