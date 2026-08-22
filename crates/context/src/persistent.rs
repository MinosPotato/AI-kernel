//! [`RedbContextStore`]: the same [`ContextStore`] contract, kept on disk.
//!
//! # What is stored, and where the truth lives
//!
//! Five tables in the shared [`Db`], all namespaced under `context.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `context.sessions` | session | owner, timestamps, next sequence, record and token totals |
//! | `context.records` | (session, sequence) | the record, minus what the key already says |
//! | `context.record_ids` | (session, record id) | the record's sequence |
//! | `context.by_owner` | (owner, session) | nothing — the key is the index |
//! | `context.by_updated` | (updated at, session) | nothing — the key is the index |
//!
//! The session is the first element of every *record* key, so a range scan of one session
//! can only ever return that session's rows. Isolation is therefore a property of the key
//! layout and not only of the ownership check above it: even a bug in the check cannot make
//! a scan wander into another conversation, because the range does not reach.
//!
//! # The two indexes, and why they are keys rather than scans
//!
//! `context.by_owner` is what makes [`ContextStore::sessions`] read only the headers a
//! caller is entitled to. The alternative — scan every session and discard the ones that do
//! not match — is not merely slower; it decodes other principals' headers on a path whose
//! whole purpose is to not reveal that they exist, and it makes the cost of one user's
//! listing depend on how many conversations every other user has had. Owner first in the
//! key means the range for a principal stops at the principal's last session.
//!
//! `context.by_updated` is the same argument for retention. A sweep asks "which sessions
//! have been idle since *t*", which without an index is a scan of every session on every
//! tick, for ever, to find the handful that are due. Timestamp first in the key turns that
//! into a bounded range from the left of the tree — and, because a batch removes exactly
//! what it read, each subsequent batch is a seek to the new leftmost key rather than a
//! rescan.
//!
//! Both are keys with no value: the key *is* the entry, so there is nothing stored in an
//! index that could disagree with the header it points at. What can still go wrong is an
//! index entry existing when the session does not, or the reverse — see
//! [reconciliation](self#reconciliation).
//!
//! # Reconciliation
//!
//! The indexes are derived data: everything in them is recoverable from `context.sessions`,
//! which remains the only authority on who owns a session. [`RedbContextStore::new`]
//! therefore reconciles them against the headers when it opens, adding what is missing and
//! removing what points nowhere.
//!
//! That is what upgrades a database written before these tables existed, without a schema
//! migration and without `aik-store` having to learn this crate's encoding — which is the
//! question [`aik_store::schema`] deliberately leaves open. It is also a repair: an index
//! that somehow drifted is corrected at the next start-up rather than quietly narrowing what
//! a listing shows. Reconciliation reads owners *from the header*, so it can only ever
//! restore the ownership already recorded; it is not a path by which a session changes hands.
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
//! Writers queue on [`Db::writes`] first, so that a burst waits on one lock rather than
//! occupying a blocking-pool thread each to wait inside `begin_write`. That queue belongs to
//! the database rather than to this store precisely because the memory store shares the file:
//! see [`Db::writes`] for why a per-subsystem queue would not hold.
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

use crate::retention::{DEFAULT_RETENTION_BATCH, RetentionSweeper};
use crate::session::{AssemblyReporter, authorize};
use crate::store::{DEFAULT_MAX_RECORDS_PER_SESSION, compaction_boundary, sort_sessions};
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

/// Owner-scoped enumeration: every session one principal owns, and nothing else.
const BY_OWNER: TableDefinition<'static, (&str, u128), ()> =
    TableDefinition::new("context.by_owner");

/// Idle-time ordering, so a retention sweep reads only what is due.
const BY_UPDATED: TableDefinition<'static, (u64, u128), ()> =
    TableDefinition::new("context.by_updated");

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

/// What compaction needs to know about a record, without paying to rebuild it.
///
/// Deserialised from the same bytes as [`StoredRecord`], minus the `message` — which is the
/// whole payload and the whole cost. `serde` walks the omitted field and discards it rather
/// than allocating it, so deciding what to remove from a ten-thousand-record session reads
/// the session once and holds three small fields per record instead of ten thousand
/// messages.
#[derive(Debug, Clone, Deserialize)]
struct RecordSummary {
    id: ContextId,
    pinned: bool,
    tokens: u64,
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
    retention_batch: usize,
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
    /// Creates the five tables if they are absent, so that later reads — which run in read
    /// transactions and cannot create anything — always find them, and reconciles the two
    /// indexes against the session headers. This is the only write the store performs that
    /// is not an append, a clear, a compaction or a sweep; the module documentation's
    /// "Reconciliation" section explains what it is for.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        open_tables(db.database())?;
        Ok(Self {
            db,
            counter: Arc::new(HeuristicTokenCounter::new()),
            clock: Arc::new(SystemClock),
            reporter: AssemblyReporter::silent(ComponentId::new(crate::DEFAULT_COMPONENT_ID)),
            max_records: DEFAULT_MAX_RECORDS_PER_SESSION,
            retention_batch: DEFAULT_RETENTION_BATCH,
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

    /// Overrides how many sessions one retention sweep removes per transaction.
    ///
    /// Mostly for tests, which need a batch small enough that a sweep is provably more than
    /// one of them. See [`DEFAULT_RETENTION_BATCH`] for why the shipped number is what it is.
    #[must_use]
    pub fn with_retention_batch(mut self, retention_batch: usize) -> Self {
        self.retention_batch = retention_batch.max(1);
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
        let principal = cx.principal_or_system();
        let session = *session;
        let now = self.clock.now();
        let max_records = self.max_records;

        let _queued = self.db.writes().lock().await;
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
        let principal = cx.principal_or_system();
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
        let principal = cx.principal_or_system();
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
        let principal = cx.principal_or_system();
        let session = *session;

        let joined =
            tokio::task::spawn_blocking(move || stats_blocking(db.database(), session, &principal))
                .await;
        finish(joined, "reading context statistics")
    }

    async fn clear(&self, session: &SessionId, cx: &ExecutionContext) -> Result<usize> {
        let db = self.db.clone();
        let principal = cx.principal_or_system();
        let session = *session;

        let _queued = self.db.writes().lock().await;
        let joined =
            tokio::task::spawn_blocking(move || clear_blocking(db.database(), session, &principal))
                .await;
        finish(joined, "clearing a context session")
    }

    async fn sessions(&self, cx: &ExecutionContext) -> Result<Vec<ContextStats>> {
        let db = self.db.clone();
        let principal = cx.principal_or_system();

        let joined =
            tokio::task::spawn_blocking(move || sessions_blocking(db.database(), &principal)).await;
        finish(joined, "listing context sessions")
    }

    async fn compact(
        &self,
        session: &SessionId,
        keep: usize,
        cx: &ExecutionContext,
    ) -> Result<usize> {
        let db = self.db.clone();
        let principal = cx.principal_or_system();
        let session = *session;

        let _queued = self.db.writes().lock().await;
        let joined = tokio::task::spawn_blocking(move || {
            compact_blocking(db.database(), session, keep, &principal)
        })
        .await;
        finish(joined, "compacting a context session")
    }
}

#[async_trait]
impl RetentionSweeper for RedbContextStore {
    /// Reclaims in batches of [`DEFAULT_RETENTION_BATCH`] sessions, looping until nothing is
    /// due.
    ///
    /// The write lock is taken and released per batch rather than held for the whole sweep,
    /// so a backlog cannot starve the conversation somebody is having; and each batch is its
    /// own transaction, so dropping this future between batches leaves the store consistent
    /// with whatever it had already reclaimed. See [`RetentionSweeper`] for why both matter.
    async fn sweep_stale(&self, cutoff: Timestamp) -> Result<usize> {
        let mut total = 0usize;
        loop {
            let db = self.db.clone();
            let batch = self.retention_batch;
            let removed = {
                let _queued = self.db.writes().lock().await;
                let joined = tokio::task::spawn_blocking(move || {
                    sweep_batch_blocking(db.database(), cutoff, batch)
                })
                .await;
                finish(joined, "sweeping stale context sessions")?
            };
            total += removed;

            // A short batch means the range ran out, so nothing is left due at `cutoff`.
            // Anything appended to since is the next sweep's business.
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

/// Creates the store's tables if they do not exist yet, and reconciles the two indexes.
fn open_tables(db: &Database) -> Result<()> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning the context schema transaction", error))?;
    {
        transaction
            .open_table(RECORDS)
            .map_err(|error| store_error("creating the context record table", error))?;
        transaction
            .open_table(RECORD_IDS)
            .map_err(|error| store_error("creating the context record index", error))?;
        reconcile_indexes(&transaction)?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing the context schema", error))
}

/// Brings `context.by_owner` and `context.by_updated` back into agreement with
/// `context.sessions`.
///
/// Both directions, because both can be wrong and neither failure is loud: a session with no
/// index entry is invisible to enumeration and immortal to retention, and an entry naming a
/// session that no longer exists makes a listing fail on a header it cannot read. The header
/// is the authority in both directions — this only ever copies what `context.sessions`
/// already says, so it cannot move a session between owners.
///
/// Run inside the caller's transaction, on every open. On a database this build has been
/// writing there is nothing to do and nothing is written; on one written before these tables
/// existed, this is the whole of the upgrade.
fn reconcile_indexes(transaction: &WriteTransaction) -> Result<()> {
    let sessions = open_write(transaction, SESSIONS, "session")?;
    let mut wanted_owner: std::collections::BTreeSet<(String, u128)> =
        std::collections::BTreeSet::new();
    let mut wanted_updated: std::collections::BTreeSet<(u64, u128)> =
        std::collections::BTreeSet::new();

    for row in sessions
        .iter()
        .map_err(|error| store_error("scanning the context session table", error))?
    {
        let (key, value) = row.map_err(|error| store_error("reading a context session", error))?;
        let header: SessionHeader = decode("context session header", value.value())?;
        wanted_owner.insert((header.owner.as_str().to_owned(), key.value()));
        wanted_updated.insert((header.updated_at.as_millis(), key.value()));
    }
    // Dropped before the indexes are opened: reconciliation is the one place that would hold
    // three write tables at once for no reason, and the session scan is finished with.
    drop(sessions);

    {
        let mut by_owner = open_write(transaction, BY_OWNER, "owner index")?;
        let present: std::collections::BTreeSet<(String, u128)> = by_owner
            .iter()
            .map_err(|error| store_error("scanning the context owner index", error))?
            .map(|row| {
                row.map(|(key, _)| {
                    let (owner, session) = key.value();
                    (owner.to_owned(), session)
                })
                .map_err(|error| store_error("reading the context owner index", error))
            })
            .collect::<Result<_>>()?;

        for entry in present.difference(&wanted_owner) {
            by_owner
                .remove((entry.0.as_str(), entry.1))
                .map_err(|error| {
                    store_error("removing a stale context owner index entry", error)
                })?;
        }
        for entry in wanted_owner.difference(&present) {
            by_owner
                .insert((entry.0.as_str(), entry.1), ())
                .map_err(|error| store_error("restoring a context owner index entry", error))?;
        }
    }

    let mut by_updated = open_write(transaction, BY_UPDATED, "updated index")?;
    let present: std::collections::BTreeSet<(u64, u128)> = by_updated
        .iter()
        .map_err(|error| store_error("scanning the context updated index", error))?
        .map(|row| {
            row.map(|(key, _)| key.value())
                .map_err(|error| store_error("reading the context updated index", error))
        })
        .collect::<Result<_>>()?;

    for entry in present.difference(&wanted_updated) {
        by_updated
            .remove(*entry)
            .map_err(|error| store_error("removing a stale context updated index entry", error))?;
    }
    for entry in wanted_updated.difference(&present) {
        by_updated
            .insert(*entry, ())
            .map_err(|error| store_error("restoring a context updated index entry", error))?;
    }

    Ok(())
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
        let existing = read_header(&sessions, key)?;
        let mut header = match existing.clone() {
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
        let previously_updated = header.updated_at;
        header.updated_at = now;
        let encoded_header = encode("context session header", &header)?;
        sessions
            .insert(key, encoded_header.as_slice())
            .map_err(|error| store_error("updating a context session", error))?;
        drop(sessions);

        // The owner index gains an entry only when the session is created; a session never
        // changes hands, so there is nothing to move on a later append.
        if existing.is_none() {
            let mut by_owner = open_write(&transaction, BY_OWNER, "owner index")?;
            by_owner
                .insert((header.owner.as_str(), key), ())
                .map_err(|error| store_error("indexing a context session by owner", error))?;
        }

        // The updated index does move, on every append: the old entry is removed and the new
        // one written in the same transaction, so a crash cannot leave the session listed
        // under two idle times — or, worse, under an idle time that is already due.
        let mut by_updated = open_write(&transaction, BY_UPDATED, "updated index")?;
        if existing.is_some() && previously_updated.as_millis() != now.as_millis() {
            by_updated
                .remove((previously_updated.as_millis(), key))
                .map_err(|error| store_error("moving a context updated index entry", error))?;
        }
        by_updated
            .insert((now.as_millis(), key), ())
            .map_err(|error| store_error("indexing a context session by idle time", error))?;

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
        drop(sessions);

        remove_session_rows(&transaction, key, &header)?;

        usize::try_from(header.records).unwrap_or(usize::MAX)
    };

    transaction
        .commit()
        .map_err(|error| store_error("committing a context clear", error))?;
    Ok(removed)
}

/// Removes everything a session owns except its header, which the caller has already taken.
///
/// Every table that names the session, in one place: the records, the record-id index, and
/// both session indexes. Sharing it between `clear` and the retention sweep is not tidiness
/// — it is the only way the two cannot come to differ about what "delete a session" means,
/// and a leftover index entry is exactly the kind of difference that shows up much later as
/// a listing that names a session nobody can read.
fn remove_session_rows(
    transaction: &WriteTransaction,
    key: u128,
    header: &SessionHeader,
) -> Result<()> {
    let mut records = open_write(transaction, RECORDS, "record")?;
    records
        .retain_in(record_range(key), |_, _| false)
        .map_err(|error| store_error("removing a context session's records", error))?;
    drop(records);

    let mut ids = open_write(transaction, RECORD_IDS, "record index")?;
    ids.retain_in(index_range(key), |_, _| false)
        .map_err(|error| store_error("removing a context session's record index", error))?;
    drop(ids);

    let mut by_owner = open_write(transaction, BY_OWNER, "owner index")?;
    by_owner
        .remove((header.owner.as_str(), key))
        .map_err(|error| store_error("removing a context owner index entry", error))?;
    drop(by_owner);

    let mut by_updated = open_write(transaction, BY_UPDATED, "updated index")?;
    by_updated
        .remove((header.updated_at.as_millis(), key))
        .map_err(|error| store_error("removing a context updated index entry", error))?;

    Ok(())
}

/// Lists the sessions `principal` may act for, reading only their headers.
///
/// The owner index bounds what is read; the `may_act_for` filter below decides what is
/// returned. Both, deliberately, and in that order: the index narrows the scan, it does not
/// define the rule, so an index entry that somehow named the wrong owner would still be
/// discarded by the check against the header — which is the authority.
fn sessions_blocking(db: &Database, principal: &Principal) -> Result<Vec<ContextStats>> {
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a context session listing", error))?;
    let sessions = open_read(&transaction, SESSIONS, "session")?;
    let by_owner = open_read(&transaction, BY_OWNER, "owner index")?;

    let mut listed = Vec::new();
    for owner in accessible_owners(principal) {
        for row in by_owner
            .range(owner_range(&owner))
            .map_err(|error| store_error("scanning the context owner index", error))?
        {
            let (key, _) =
                row.map_err(|error| store_error("reading the context owner index", error))?;
            let (_, session_key) = key.value();
            let session = session_from_key(session_key);
            let header = read_header(&sessions, session_key)?.ok_or_else(|| {
                // Both are written in one transaction and reconciled on open, so this means
                // the database was edited by something else. Skipping it would hide that.
                Error::other(format!(
                    "the context owner index names session `{session}` under owner `{owner}`, \
                     but no session is stored under that id"
                ))
            })?;
            if !principal.may_act_for(&header.owner) {
                continue;
            }
            listed.push(ContextStats {
                session,
                owner: header.owner,
                records: usize::try_from(header.records).unwrap_or(usize::MAX),
                tokens: header.tokens,
                created_at: header.created_at,
                updated_at: header.updated_at,
            });
        }
    }

    sort_sessions(&mut listed);
    Ok(listed)
}

/// The owners a principal's own enumeration may read: itself, and whoever it acts for.
///
/// Deliberately *not* a re-derivation of the rule — [`Principal::may_act_for`] is still what
/// decides, below. This only says which ranges are worth opening, and it is correct to
/// enumerate exactly because `may_act_for` accepts exactly these two.
fn accessible_owners(principal: &Principal) -> Vec<PrincipalId> {
    let mut owners = vec![principal.id.clone()];
    if let Some(delegator) = &principal.on_behalf_of
        && delegator != &principal.id
    {
        owners.push(delegator.clone());
    }
    owners
}

/// Removes the oldest unpinned records of a session, keeping the newest `keep`.
///
/// One transaction: the records, their id-index entries and the header's totals move
/// together or not at all. The header's timestamps and `next_sequence` are deliberately
/// untouched — sequence numbers are never reused, and compaction is not activity.
fn compact_blocking(
    db: &Database,
    session: SessionId,
    keep: usize,
    principal: &Principal,
) -> Result<usize> {
    let key = session_key(session);
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a context compaction", error))?;

    let removed = {
        let mut sessions = open_write(&transaction, SESSIONS, "session")?;
        let Some(mut header) = read_header(&sessions, key)? else {
            // Nothing to do, and nothing written: the transaction is dropped, not committed.
            return Ok(0);
        };
        authorize(&session, &header.owner, principal)?;

        // Read summaries rather than records: what compaction needs per record is three
        // small fields, and a session at the record bound holds ten thousand messages it
        // would otherwise rebuild in full only to drop them.
        let summaries = {
            let records = open_write(&transaction, RECORDS, "record")?;
            let mut summaries: Vec<(u64, RecordSummary)> = Vec::new();
            for row in records
                .range(record_range(key))
                .map_err(|error| store_error("scanning a context session", error))?
            {
                let (stored_key, value) =
                    row.map_err(|error| store_error("reading a context record", error))?;
                let summary: RecordSummary = decode("context record", value.value())?;
                summaries.push((stored_key.value().1, summary));
            }
            summaries
        };

        let boundary =
            compaction_boundary(summaries.iter().map(|(_, summary)| summary.pinned), keep);
        if boundary == 0 {
            return Ok(0);
        }

        let doomed: Vec<(u64, RecordSummary)> = summaries
            .into_iter()
            .take(boundary)
            .filter(|(_, summary)| !summary.pinned)
            .collect();
        if doomed.is_empty() {
            return Ok(0);
        }

        let reclaimed: u64 = doomed.iter().map(|(_, summary)| summary.tokens).sum();
        {
            let mut records = open_write(&transaction, RECORDS, "record")?;
            let mut ids = open_write(&transaction, RECORD_IDS, "record index")?;
            for (sequence, summary) in &doomed {
                records
                    .remove((key, *sequence))
                    .map_err(|error| store_error("removing a compacted context record", error))?;
                ids.remove((key, record_key(summary.id))).map_err(|error| {
                    store_error("removing a compacted context record index entry", error)
                })?;
            }
        }

        let count = doomed.len();
        header.records = header.records.saturating_sub(count as u64);
        header.tokens = header.tokens.saturating_sub(reclaimed);
        let encoded_header = encode("context session header", &header)?;
        sessions
            .insert(key, encoded_header.as_slice())
            .map_err(|error| store_error("updating a context session", error))?;

        count
    };

    transaction
        .commit()
        .map_err(|error| store_error("committing a context compaction", error))?;
    Ok(removed)
}

/// Removes up to `limit` sessions last appended to at or before `cutoff`, in one transaction.
///
/// The caller loops until a batch comes back short — see [`RetentionSweeper::sweep_stale`].
/// Each batch restarts the range scan at the left of `context.by_updated`, which is a seek to
/// the leftmost remaining key rather than a rescan: the entries the previous batch removed
/// are no longer there to be skipped.
///
/// An index entry naming a session `context.sessions` no longer holds is reported as an error
/// rather than skipped, for the same reason a dangling record-id entry is: it means the
/// database was edited by something that is not this store, and a sweep that quietly tolerated
/// it would keep the inconsistency alive for ever.
fn sweep_batch_blocking(db: &Database, cutoff: Timestamp, limit: usize) -> Result<usize> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a context retention sweep", error))?;

    let due: Vec<(u64, u128)> = {
        let by_updated = open_write(&transaction, BY_UPDATED, "updated index")?;
        let mut due = Vec::new();
        for row in by_updated
            .range(..=(cutoff.as_millis(), u128::MAX))
            .map_err(|error| store_error("scanning the context updated index", error))?
        {
            let (key, _) =
                row.map_err(|error| store_error("reading the context updated index", error))?;
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
    for (updated_ms, session_key) in due {
        let session = session_from_key(session_key);
        let header = {
            let mut sessions = open_write(&transaction, SESSIONS, "session")?;
            let value = sessions
                .remove(session_key)
                .map_err(|error| store_error("removing a stale context session", error))?
                .ok_or_else(|| {
                    Error::other(format!(
                        "the context updated index names session `{session}` at {updated_ms}ms, \
                         but no session is stored under that id"
                    ))
                })?;
            decode::<SessionHeader>("context session header", value.value())?
        };
        remove_session_rows(&transaction, session_key, &header)?;
        removed += 1;
    }

    transaction
        .commit()
        .map_err(|error| store_error("committing a context retention sweep", error))?;
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

/// Reconstructs a session id from the raw key it was found under.
fn session_from_key(key: u128) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::from_u128(key))
}

/// Every `context.by_owner` entry for one owner, and nothing else.
fn owner_range(owner: &PrincipalId) -> std::ops::RangeInclusive<(&str, u128)> {
    (owner.as_str(), 0)..=(owner.as_str(), u128::MAX)
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
