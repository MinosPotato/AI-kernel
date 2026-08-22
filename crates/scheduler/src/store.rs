//! [`JobStore`]: where a persistent job's definition and schedule position are kept.
//!
//! The scheduler holds its whole schedule in memory and always has: a store is a *durable
//! mirror* of the [`persistent`](aik_api::scheduler::JobSpec::persistent) subset of it, not
//! the schedule itself. That asymmetry is deliberate and is what keeps the two schedulers one
//! implementation rather than two:
//!
//! * every firing decision is made against memory, so a scheduler with no store behaves
//!   identically to one with a store that happens to hold no persistent jobs;
//! * a volatile job never reaches the disk, however the scheduler is wired;
//! * the store is read exactly once, at startup, and written only when a persistent job is
//!   scheduled, cancelled or claims a firing.
//!
//! # Ordering
//!
//! Every mutation writes the store *before* memory, and abandons the whole operation if the
//! write fails. A job that a caller was told is scheduled is therefore a job that is on disk,
//! and the failure mode is "the schedule did not change" rather than "the schedule changed
//! here but not there".

use aik_api::scheduler::JobId;
use aik_core::Result;
use async_trait::async_trait;

use crate::state::JobState;

/// A durable home for persistent jobs.
///
/// # Obligations
///
/// * **Durable on return.** When [`JobStore::put`] returns `Ok`, the job survives losing the
///   process. The scheduler relies on this for at-most-once firing: it claims a firing by
///   writing the advanced schedule *before* the handler runs, so a store that acknowledged a
///   write it had not committed would turn a crash into a repeat.
/// * **Total.** [`JobStore::remove`] succeeds whether or not the job was there, so cancelling
///   a job that a concurrent restart already removed is not an error.
/// * **Exact.** [`JobStore::load`] returns what was written, or fails. A row it cannot decode
///   is corruption and must be reported, never skipped: a scheduler that quietly forgot a job
///   it could not read would be indistinguishable from one with nothing to do.
#[async_trait]
pub trait JobStore: Send + Sync + std::fmt::Debug + 'static {
    /// Reads every persisted job. Called once, during startup.
    async fn load(&self) -> Result<Vec<JobState>>;

    /// Writes one job, replacing any job already stored under its id.
    async fn put(&self, job: &JobState) -> Result<()>;

    /// Removes one job, if it is there.
    async fn remove(&self, id: &JobId) -> Result<()>;
}
