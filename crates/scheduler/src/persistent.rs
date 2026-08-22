//! [`RedbJobStore`]: the schedule, kept in the kernel's shared database.
//!
//! # What is stored, and where the truth lives
//!
//! One table in the shared [`Db`], namespaced under `sched.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `sched.jobs` | job id | owner, trigger, handler, payload, retry, timeout, next and last run |
//!
//! A job's own id is the primary key, so it is not repeated inside the stored value — the
//! same rule the memory store follows, and for the same reason: a key and a value that can
//! disagree about which job this is are a class of inconsistency that cannot arise if the
//! value never says. Neither is
//! [`persistent`](aik_api::scheduler::JobSpec::persistent) stored, for the same reason once
//! removed: everything in this table is persistent by construction, and a stored `false`
//! could only ever be a contradiction.
//!
//! # Why there is no index
//!
//! The memory store indexes by kind, expiry and owner because it holds more records than it
//! can afford to read. A schedule is the opposite kind of collection: it is small, every job
//! in it is live, and the scheduler keeps all of it resident because it has to know what is
//! due next. So this table is read exactly once — at startup, in full — and after that it is
//! written to and never scanned. An index would cost writes to accelerate a scan that does
//! not happen.
//!
//! # Atomicity and blocking
//!
//! Each operation is one redb transaction, opened and committed inside a single
//! [`spawn_blocking`](tokio::task::spawn_blocking) closure, so a transaction is never held
//! across an await. Writers queue on [`Db::writes`] first — the database's queue, not this
//! store's, because the memory and transcript stores write to the same file.

use std::sync::Arc;

use aik_api::scheduler::{JobId, JobSpec};
use aik_core::{Error, Result};
use aik_store::redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use aik_store::{Db, store_error};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::JobState;
use crate::store::JobStore;

/// One row per persistent job, keyed by its stable name.
const JOBS: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("sched.jobs");

/// A job, as stored: everything a [`JobState`] carries except the id it is keyed by and the
/// `persistent` flag every row in this table necessarily has set.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredJob {
    owner: aik_api::permission::Principal,
    trigger: aik_api::scheduler::Trigger,
    handler: aik_core::ComponentId,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    payload: Value,
    #[serde(default)]
    retry: aik_api::scheduler::RetryPolicy,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_run: Option<aik_core::clock::Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run: Option<aik_core::clock::Timestamp>,
}

impl StoredJob {
    fn from_state(state: &JobState) -> Self {
        Self {
            owner: state.owner.clone(),
            trigger: state.spec.trigger.clone(),
            handler: state.spec.handler.clone(),
            payload: state.spec.payload.clone(),
            retry: state.spec.retry,
            timeout_ms: state
                .spec
                .timeout
                .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
            next_run: state.next_run,
            last_run: state.last_run,
        }
    }

    fn into_state(self, id: JobId) -> JobState {
        let mut spec = JobSpec::new(id, self.trigger, self.handler);
        spec.payload = self.payload;
        spec.persistent = true;
        spec.retry = self.retry;
        spec.timeout = self.timeout_ms.map(std::time::Duration::from_millis);
        JobState {
            spec,
            owner: self.owner,
            next_run: self.next_run,
            last_run: self.last_run,
        }
    }
}

/// A [`JobStore`] backed by the kernel's shared [`Db`].
///
/// # Security
///
/// The database file is created `0600` inside a `0700` directory by [`Db::open`] — see
/// [`aik_store`]. A schedule is worth that on its own: a job payload is caller-authored and a
/// job's owner is an authority record, and either would tell another local account rather
/// more about what this system does on whose behalf than it should.
pub struct RedbJobStore {
    db: Arc<Db>,
}

impl std::fmt::Debug for RedbJobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbJobStore")
            .field("path", &self.db.path())
            .finish()
    }
}

impl RedbJobStore {
    /// Opens the store over an already-open database.
    ///
    /// Creates the table if it is absent, so that the startup read — which runs in a read
    /// transaction and cannot create anything — always finds it.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        create_tables(db.database())?;
        Ok(Self { db })
    }

    /// The database this store writes to.
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }
}

#[async_trait]
impl JobStore for RedbJobStore {
    async fn load(&self) -> Result<Vec<JobState>> {
        let db = self.db.clone();
        let joined = tokio::task::spawn_blocking(move || load_blocking(db.database())).await;
        finish(joined, "reading the schedule")
    }

    async fn put(&self, job: &JobState) -> Result<()> {
        let db = self.db.clone();
        let id = job.spec.id.clone();
        let stored = StoredJob::from_state(job);
        let _queued = self.db.writes().lock().await;
        let joined =
            tokio::task::spawn_blocking(move || put_blocking(db.database(), &id, &stored)).await;
        finish(joined, "writing a scheduled job")
    }

    async fn remove(&self, id: &JobId) -> Result<()> {
        let db = self.db.clone();
        let id = id.clone();
        let _queued = self.db.writes().lock().await;
        let joined = tokio::task::spawn_blocking(move || remove_blocking(db.database(), &id)).await;
        finish(joined, "removing a scheduled job")
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

/// Creates the store's table if it does not exist yet.
fn create_tables(db: &Database) -> Result<()> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning the schedule schema transaction", error))?;
    transaction
        .open_table(JOBS)
        .map_err(|error| store_error("creating the scheduled job table", error))?;
    transaction
        .commit()
        .map_err(|error| store_error("committing the schedule schema", error))
}

/// Reads the whole table.
///
/// A row that will not decode fails the load rather than being skipped. A scheduler that
/// silently dropped the jobs it could not read would start up looking perfectly healthy while
/// having forgotten what it was told to do, which is the failure mode this whole subsystem
/// exists to prevent.
fn load_blocking(db: &Database) -> Result<Vec<JobState>> {
    let transaction = db
        .begin_read()
        .map_err(|error| store_error("beginning a schedule read", error))?;
    let table = transaction
        .open_table(JOBS)
        .map_err(|error| store_error("opening the scheduled job table", error))?;

    let mut jobs = Vec::new();
    for row in table
        .iter()
        .map_err(|error| store_error("scanning the scheduled job table", error))?
    {
        let (key, value) = row.map_err(|error| store_error("reading a scheduled job", error))?;
        let id = JobId::new(key.value());
        let stored: StoredJob = decode(&id, value.value())?;
        jobs.push(stored.into_state(id));
    }
    Ok(jobs)
}

/// Upserts one job.
fn put_blocking(db: &Database, id: &JobId, job: &StoredJob) -> Result<()> {
    let encoded = serde_json::to_vec(job)
        .map_err(|error| Error::wrap(format!("encoding the scheduled job `{id}`"), error))?;

    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a schedule write", error))?;
    {
        let mut table = transaction
            .open_table(JOBS)
            .map_err(|error| store_error("opening the scheduled job table", error))?;
        table
            .insert(id.as_str(), encoded.as_slice())
            .map_err(|error| store_error("writing a scheduled job", error))?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing a schedule write", error))
}

/// Removes one job, whether or not it was there.
fn remove_blocking(db: &Database, id: &JobId) -> Result<()> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning a schedule removal", error))?;
    {
        let mut table = transaction
            .open_table(JOBS)
            .map_err(|error| store_error("opening the scheduled job table", error))?;
        table
            .remove(id.as_str())
            .map_err(|error| store_error("removing a scheduled job", error))?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing a schedule removal", error))
}

/// Decodes a stored job, naming it in the failure.
///
/// A failure here means the bytes on disk are not what this build writes: corrupt or foreign
/// data, not an absent job. Reporting it is the whole point — see [`load_blocking`].
fn decode(id: &JobId, bytes: &[u8]) -> Result<StoredJob> {
    serde_json::from_slice(bytes)
        .map_err(|error| Error::wrap(format!("decoding the scheduled job `{id}`"), error))
}
