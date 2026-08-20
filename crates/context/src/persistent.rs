//! [`RedbContextStore`]: the same [`ContextStore`] contract, kept on disk.
//!
//! # What is stored, and where the truth lives
//!
//! Three tables in the shared [`Db`], all namespaced under `context.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `context.sessions` | session | owner, timestamps, next sequence, record and token totals |
//! | `context.records` | (session, sequence) | the record, minus what the key already says |
//! | `context.record_ids` | (session, record id) | the record's sequence |
//!
//! The session is the first element of every key, so a range scan of one session can only
//! ever return that session's rows. Isolation is therefore a property of the key layout and
//! not only of the ownership check above it: even a bug in the check cannot make a scan
//! wander into another conversation, because the range does not reach.
//!
//! A record's session and sequence live in its key alone and are reconstructed on read.
//! Storing them a second time in the value would create a class of inconsistency — key and
//! value disagreeing about where a record sits — that cannot arise if the value never says.
//!
//! `context.record_ids` exists because [`ContextStore::get`] addresses a record by
//! [`ContextId`] while ordering needs it addressed by sequence. Keying it by
//! (session, id) rather than by id alone is what makes a valid id from *another* session
//! simply absent rather than found-then-refused.
//!
//! # Atomicity
//!
//! Every mutation is one redb write transaction covering the record, both indexes and the
//! session header together. A crash between the record and the header it accounts for is
//! not a state this store can be in: either the transaction committed and both are there,
//! or it did not and neither is. That is the whole reason the totals are stored rather than
//! recomputed — a stored total is only safe if it cannot drift from what it counts.
//!
//! # Blocking
//!
//! redb is synchronous. Each method moves its entire transaction onto a blocking thread
//! with [`spawn_blocking`](tokio::task::spawn_blocking) and awaits the result, so a
//! transaction is opened and committed within one closure and is never held across an
//! await — which would pin redb's single write slot for as long as the async task took to
//! be polled again.
//!
//! # Reading a whole session to build a window
//!
//! [`ContextStore::window`] loads every record of the session and hands them to the same
//! `assemble` the in-memory store uses. That is deliberate: assembly must see the whole
//! transcript to keep pinned records, to select a contiguous recent run, and to account for
//! what it dropped. A session is bounded by
//! [`DEFAULT_MAX_RECORDS_PER_SESSION`](crate::DEFAULT_MAX_RECORDS_PER_SESSION), so the read
//! is bounded too, and it is the same working set the in-memory store holds permanently.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{
    ContextBudget, ContextEntry, ContextId, ContextRecord, ContextStats, ContextStore,
    ContextWindow, TokenCounter,
};
use aik_api::execution::ExecutionContext;
use aik_api::model::Message;
use aik_api::permission::{Principal, PrincipalId};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::event::EventBus;
use aik_core::id::ComponentId;
use aik_core::{Error, Result};
use aik_store::redb::{
    Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};
use aik_store::{Db, store_error};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::session::{AssemblyReporter, authorize, principal_of};
use crate::store::DEFAULT_MAX_RECORDS_PER_SESSION;
use crate::tokens::HeuristicTokenCounter;
use crate::window::assemble;

/// One row per session: who owns it and what it adds up to.
const SESSIONS: TableDefinition<'static, u128, &[u8]> = TableDefinition::new("context.sessions");

/// One row per record, ordered within its session by sequence.
const RECORDS: TableDefinition<'static, (u128, u64), &[u8]> =
    TableDefinition::new("context.records");

/// Session-scoped lookup from a record's id to its sequence.
const RECORD_IDS: TableDefinition<'static, (u128, u128), u64> =
    TableDefinition::new("context.record_ids");

/// A session's header, as stored.
///
/// The totals are maintained transactionally alongside the records they describe, so
/// [`ContextStore::stats`] is a single row read rather than a scan of the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    owner: PrincipalId,
    created_at: Timestamp,
    updated_at: Timestamp,
    next_sequence: u64,
    records: u64,
    tokens: u64,
}

/// A record, as stored: everything a [`ContextRecord`] carries except its session and
/// sequence, which are its key.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRecord {
    id: ContextId,
    message: Message,
    pinned: bool,
    principal: PrincipalId,
    created_at: Timestamp,
    tokens: u64,
}

impl StoredRecord {
    /// Rebuilds the full record, taking its position from the key it was found under.
    fn into_record(self, session: SessionId, sequence: u64) -> ContextRecord {
        ContextRecord {
            id: self.id,
            session,
            sequence,
            message: self.message,
            pinned: self.pinned,
            principal: self.principal,
            created_at: self.created_at,
            tokens: self.tokens,
        }
    }
}

/// A [`ContextStore`] that keeps sessions in the kernel's shared [`Db`].
///
/// It is the same contract as [`InMemoryContextStore`](crate::InMemoryContextStore), with
/// the same guarantees — append-only, attributed by the store, session-scoped retrieval,
/// owned sessions, bounded — and one more: a restart does not lose the conversation. Both
/// implementations run the same conformance tests, because "persistent" must not mean
/// "subtly different".
///
/// Windows are assembled by the same code in both, so a budget applied to a reopened
/// transcript produces exactly the window it would have produced before the restart.
///
/// # Security
///
/// The database file is created `0600` inside a `0700` directory by
/// [`Db::open`] — see [`aik_store`] — which matters more here than for any other subsystem,
/// because this is the file that holds the conversations. Nothing in this crate widens
/// that.
pub struct RedbContextStore {
    db: Arc<Db>,
    counter: Arc<dyn TokenCounter>,
    clock: SharedClock,
    reporter: AssemblyReporter,
    max_records: usize,
    /// Serialises writers before they reach a blocking thread.
    ///
    /// redb already allows only one write transaction at a time and blocks the rest, but it
    /// blocks them *inside* `begin_write`, on a thread from tokio's blocking pool. Queueing
    /// here instead means a burst of concurrent appends occupies one pool thread rather than
    /// one per caller, leaving the pool for the work that can actually proceed — filesystem
    /// tools, mostly. It costs no throughput: the writes were going to be serialised anyway.
    writes: Mutex<()>,
}

impl std::fmt::Debug for RedbContextStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbContextStore")
            .field("path", &self.db.path())
            .field("max_records_per_session", &self.max_records)
            .field("events_configured", &self.reporter.is_configured())
            .finish()
    }
}

impl RedbContextStore {
    /// Opens the store over an already-open database.
    ///
    /// Creates the three tables if they are absent, so that later reads — which run in read
    /// transactions and cannot create anything — always find them. This is the only write
    /// the store performs that is not an append or a clear.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        create_tables(db.database())?;
        Ok(Self {
            db,
            counter: Arc::new(HeuristicTokenCounter::new()),
            clock: Arc::new(SystemClock),
            reporter: AssemblyReporter::silent(ComponentId::new(crate::DEFAULT_COMPONENT_ID)),
            max_records: DEFAULT_MAX_RECORDS_PER_SESSION,
            writes: Mutex::new(()),
        })
    }

    /// Uses a different token counter.
    ///
    /// Records already in the database keep the cost they were stored with; only new
    /// appends are measured by the new counter. Assembly uses the new one throughout, which
    /// is why [`TokenCounter`] requires determinism rather than agreement between
    /// implementations.
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = counter;
        self
    }

    /// Overrides the clock used to stamp records. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Publishes [`ContextAssembled`](aik_api::context::ContextAssembled) events to the
    /// kernel event bus, attributed to `source`.
    #[must_use]
    pub fn with_events(mut self, events: EventBus, source: ComponentId) -> Self {
        self.reporter = AssemblyReporter::new(events, source);
        self
    }

    /// Overrides how many records one session may hold.
    #[must_use]
    pub fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }

    /// The token counter in use, for a caller that needs to estimate before appending.
    pub fn token_counter(&self) -> &Arc<dyn TokenCounter> {
        &self.counter
    }

    /// The database this store writes to.
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }
}

#[async_trait]
impl ContextStore for RedbContextStore {
    async fn append(
        &self,
        session: &SessionId,
        entry: ContextEntry,
        cx: &ExecutionContext,
    ) -> Result<ContextRecord> {
        let db = self.db.clone();
        let counter = self.counter.clone();
        let principal = principal_of(cx);
        let session = *session;
        let now = self.clock.now();
        let max_records = self.max_records;

        let _queued = self.writes.lock().await;
        let joined = tokio::task::spawn_blocking(move || {
            append_blocking(
                db.database(),
                counter.as_ref(),
                session,
                entry,
                &principal,
                now,
                max_records,
            )
        })
        .await;
        finish(joined, "appending a context record")
    }

    async fn get(
        &self,
        session: &SessionId,
        id: &ContextId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextRecord>> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let session = *session;
        let id = *id;

        let joined = tokio::task::spawn_blocking(move || {
            get_blocking(db.database(), session, id, &principal)
        })
        .await;
        finish(joined, "reading a context record")
    }

    async fn window(
        &self,
        session: &SessionId,
        budget: &ContextBudget,
        cx: &ExecutionContext,
    ) -> Result<ContextWindow> {
        let db = self.db.clone();
        let counter = self.counter.clone();
        let principal = principal_of(cx);
        let session = *session;
        let budget = *budget;

        let joined = tokio::task::spawn_blocking(move || {
            let Some(records) = read_session_blocking(db.database(), session, &principal)? else {
                return Ok(None);
            };
            // Assembly is pure CPU work over records already read, and the transaction is
            // gone by now: nothing is held while it runs.
            Ok(Some(assemble(&records, &budget, counter.as_ref())))
        })
        .await;

        let Some(window) = finish(joined, "assembling a context window")? else {
            return Ok(ContextWindow::empty());
        };
        self.reporter
            .report(cx, session, self.clock.now(), window.usage);
        Ok(window)
    }

    async fn stats(
        &self,
        session: &SessionId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextStats>> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let session = *session;

        let joined =
            tokio::task::spawn_blocking(move || stats_blocking(db.database(), session, &principal))
                .await;
        finish(joined, "reading context statistics")
    }

    async fn clear(&self, session: &SessionId, cx: &ExecutionContext) -> Result<usize> {
        let db = self.db.clone();
        let principal = principal_of(cx);
        let session = *session;

        let _queued = self.writes.lock().await;
        let joined =
            tokio::task::spawn_blocking(move || clear_blocking(db.database(), session, &principal))
                .await;
        finish(joined, "clearing a context session")
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
        .map_err(|error| store_error("beginning the context schema transaction", error))?;
    {
        transaction
            .open_table(SESSIONS)
            .map_err(|error| store_error("creating the context session table", error))?;
        transaction
            .open_table(RECORDS)
            .map_err(|error| store_error("creating the context record table", error))?;
        transaction
            .open_table(RECORD_IDS)
            .map_err(|error| store_error("creating the context record index", error))?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing the context schema", error))
}

/// Appends one record and updates the session header, in a single transaction.
fn append_blocking(
    db: &Database,
    counter: &dyn TokenCounter,
    session: SessionId,
    entry: ContextEntry,
    principal: &Principal,
    now: Timestamp,
    max_records: usize,
) -> Result<ContextRecord> {
    // Measured before the transaction opens: an arbitrarily large message must not hold
    // redb's single write slot while it is being counted.
    let tokens = counter.count_message(&entry.message);
    let key = session_key(session);

    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a context append", error))?;

    let record = {
        let mut sessions = open_write(&transaction, SESSIONS, "session")?;
        let mut header = match read_header(&sessions, key)? {
            Some(header) => {
                authorize(&session, &header.owner, principal)?;
                header
            }
            None => SessionHeader {
                owner: principal.id.clone(),
                created_at: now,
                updated_at: now,
                next_sequence: 0,
                records: 0,
                tokens: 0,
            },
        };

        if header.records >= max_records as u64 {
            return Err(Error::other(format!(
                "context session `{session}` is full at {max_records} records; compact or clear it"
            )));
        }

        let sequence = header.next_sequence;
        let stored = StoredRecord {
            id: ContextId::new(),
            message: entry.message,
            pinned: entry.pinned,
            principal: principal.id.clone(),
            created_at: now,
            tokens,
        };
        let id_key = record_key(stored.id);
        let encoded = encode("context record", &stored)?;

        let mut records = open_write(&transaction, RECORDS, "record")?;
        records
            .insert((key, sequence), encoded.as_slice())
            .map_err(|error| store_error("writing a context record", error))?;

        let mut ids = open_write(&transaction, RECORD_IDS, "record index")?;
        ids.insert((key, id_key), sequence)
            .map_err(|error| store_error("indexing a context record", error))?;

        header.next_sequence += 1;
        header.records += 1;
        header.tokens += tokens;
        header.updated_at = now;
        let encoded_header = encode("context session header", &header)?;
        sessions
            .insert(key, encoded_header.as_slice())
            .map_err(|error| store_error("updating a context session", error))?;

        stored.into_record(session, sequence)
    };

    transaction
        .commit()
        .map_err(|error| store_error("committing a context append", error))?;
    Ok(record)
}

/// Reads one record by id, within one session.
fn get_blocking(
    db: &Database,
    session: SessionId,
    id: ContextId,
    principal: &Principal,
) -> Result<Option<ContextRecord>> {
    let key = session_key(session);
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a context read", error))?;

    let sessions = open_read(&transaction, SESSIONS, "session")?;
    let Some(header) = read_header(&sessions, key)? else {
        return Ok(None);
    };
    authorize(&session, &header.owner, principal)?;

    let ids = open_read(&transaction, RECORD_IDS, "record index")?;
    let Some(sequence) = ids
        .get((key, record_key(id)))
        .map_err(|error| store_error("reading the context record index", error))?
        .map(|value| value.value())
    else {
        return Ok(None);
    };

    let records = open_read(&transaction, RECORDS, "record")?;
    let stored = records
        .get((key, sequence))
        .map_err(|error| store_error("reading a context record", error))?
        .map(|value| decode::<StoredRecord>("context record", value.value()))
        .transpose()?
        // The index named a sequence the record table does not hold. Both are written in
        // one transaction, so this cannot happen through this code; it means the database
        // was edited by something else. Reporting "no such record" would hide that.
        .ok_or_else(|| {
            Error::other(format!(
                "context session `{session}` indexes record `{id}` at sequence {sequence}, \
                 but no record is stored there"
            ))
        })?;

    Ok(Some(stored.into_record(session, sequence)))
}

/// Reads every record of a session in append order, or `None` if there is no such session.
fn read_session_blocking(
    db: &Database,
    session: SessionId,
    principal: &Principal,
) -> Result<Option<Vec<ContextRecord>>> {
    let key = session_key(session);
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a context read", error))?;

    let sessions = open_read(&transaction, SESSIONS, "session")?;
    let Some(header) = read_header(&sessions, key)? else {
        return Ok(None);
    };
    authorize(&session, &header.owner, principal)?;

    let table = open_read(&transaction, RECORDS, "record")?;
    let mut records = Vec::with_capacity(header.records as usize);
    for row in table
        .range(record_range(key))
        .map_err(|error| store_error("scanning a context session", error))?
    {
        let (stored_key, value) =
            row.map_err(|error| store_error("reading a context record", error))?;
        let sequence = stored_key.value().1;
        let stored: StoredRecord = decode("context record", value.value())?;
        records.push(stored.into_record(session, sequence));
    }

    Ok(Some(records))
}

/// Reads a session's totals.
fn stats_blocking(
    db: &Database,
    session: SessionId,
    principal: &Principal,
) -> Result<Option<ContextStats>> {
    let key = session_key(session);
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a context read", error))?;

    let sessions = open_read(&transaction, SESSIONS, "session")?;
    let Some(header) = read_header(&sessions, key)? else {
        return Ok(None);
    };
    authorize(&session, &header.owner, principal)?;

    Ok(Some(ContextStats {
        session,
        owner: header.owner,
        records: usize::try_from(header.records).unwrap_or(usize::MAX),
        tokens: header.tokens,
        created_at: header.created_at,
        updated_at: header.updated_at,
    }))
}

/// Removes a session, its records and its index, in a single transaction.
fn clear_blocking(db: &Database, session: SessionId, principal: &Principal) -> Result<usize> {
    let key = session_key(session);
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a context clear", error))?;

    let removed = {
        let mut sessions = open_write(&transaction, SESSIONS, "session")?;
        let Some(header) = read_header(&sessions, key)? else {
            // Nothing to do, and nothing written: the transaction is dropped, not committed.
            return Ok(0);
        };
        authorize(&session, &header.owner, principal)?;

        sessions
            .remove(key)
            .map_err(|error| store_error("removing a context session", error))?;

        let mut records = open_write(&transaction, RECORDS, "record")?;
        records
            .retain_in(record_range(key), |_, _| false)
            .map_err(|error| store_error("removing a context session's records", error))?;

        let mut ids = open_write(&transaction, RECORD_IDS, "record index")?;
        ids.retain_in(index_range(key), |_, _| false)
            .map_err(|error| store_error("removing a context session's record index", error))?;

        usize::try_from(header.records).unwrap_or(usize::MAX)
    };

    transaction
        .commit()
        .map_err(|error| store_error("committing a context clear", error))?;
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
        .map_err(|error| store_error(&format!("opening the context {what} table"), error))
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
        .map_err(|error| store_error(&format!("opening the context {what} table"), error))
}

/// Reads and decodes a session header.
fn read_header<T>(sessions: &T, key: u128) -> Result<Option<SessionHeader>>
where
    T: ReadableTable<u128, &'static [u8]>,
{
    sessions
        .get(key)
        .map_err(|error| store_error("reading a context session", error))?
        .map(|value| decode("context session header", value.value()))
        .transpose()
}

/// The key a session's rows are stored under.
fn session_key(session: SessionId) -> u128 {
    session.as_uuid().as_u128()
}

/// The second element of a record index key.
fn record_key(id: ContextId) -> u128 {
    id.as_uuid().as_u128()
}

/// Every record of one session, and nothing else.
fn record_range(session: u128) -> std::ops::RangeInclusive<(u128, u64)> {
    (session, 0)..=(session, u64::MAX)
}

/// Every index entry of one session, and nothing else.
fn index_range(session: u128) -> std::ops::RangeInclusive<(u128, u128)> {
    (session, 0)..=(session, u128::MAX)
}

/// Encodes a stored value, attributing a failure to what was being written.
fn encode<T: Serialize>(what: &'static str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| Error::wrap(format!("encoding the {what}"), error))
}

/// Decodes a stored value.
///
/// A failure here means the bytes on disk are not what this build writes. That is corrupt
/// or foreign data, not a missing value, so it is an error rather than a `None`: silently
/// treating an unreadable transcript as an empty one would let a conversation continue on
/// top of history it cannot see.
fn decode<T: for<'de> Deserialize<'de>>(what: &'static str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| Error::wrap(format!("decoding the {what}"), error))
}
