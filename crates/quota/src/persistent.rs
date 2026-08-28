//! [`RedbUsageLedger`]: the same counters, kept on disk.
//!
//! # What is stored
//!
//! One table in the shared [`Db`], namespaced under `quota.`:
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `quota.usage` | (subject, window) | the [`Counters`] for that window |
//!
//! The key is the whole identity of a row, so nothing inside the value repeats it and a key
//! and a value cannot disagree about whose usage this is. Both halves of the key are text,
//! which makes one subject's rows a contiguous range and one period's windows a contiguous
//! range inside that — see [`QuotaPeriod::prefix`].
//!
//! # Why this table is small, and stays small
//!
//! Every write drops the closed windows of the period it just wrote, in the same
//! transaction, so a subject holds exactly one row per period it is capped on: a deployment
//! with two rules and ten principals holds twenty rows after a year of use, not twenty per
//! day. That is deliberate. A ledger is what enforcement reads, not a record of what
//! happened — the [audit trail](aik_api::audit) is that, it is written for every tool call
//! and authorization decision already, and it is the collection with retention rules of its
//! own precisely because it is the one nobody may quietly shorten.
//!
//! # Why a restart does not reset it
//!
//! That is the whole point of the durable backend. A per-run ceiling resets with the run and
//! a volatile ledger resets with the process, so on a host that restarts — a crash, a
//! redeploy, a machine that anybody can reboot — the only ceiling that still means anything
//! after the restart is one that outlived it.
//!
//! # Blocking
//!
//! redb is synchronous. Each method moves its whole transaction onto a blocking thread and
//! awaits the result, so a transaction is opened and committed inside one closure and is
//! never held across an await. Writers queue on [`Db::writes`] first — the database's queue,
//! not this ledger's, since the transcript, memory, the schedule and the audit trail write to
//! the same file.

use std::sync::Arc;

use aik_core::{Error, Result};
use aik_store::redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition,
};
use aik_store::{Db, store_error};
use async_trait::async_trait;

use crate::ledger::{Counters, UsageLedger};
use crate::period::QuotaPeriod;

/// One row per (subject, window).
const USAGE: TableDefinition<'static, (&str, &str), &[u8]> = TableDefinition::new("quota.usage");

/// A [`UsageLedger`] kept in the kernel's shared [`Db`].
///
/// # Security
///
/// The database file is created `0600` inside a `0700` directory by [`Db::open`]. For this
/// table that matters in one specific way: a subject that could edit its own row could grant
/// itself an unbounded budget without any authorization decision being recorded anywhere.
/// Nothing in this process exposes the ledger as a [`Tool`](aik_api::tool::Tool), and nothing
/// should: there is no path from model output to a counter that does not go through trusted
/// code deciding that a turn was taken.
#[derive(Debug)]
pub struct RedbUsageLedger {
    db: Arc<Db>,
}

impl RedbUsageLedger {
    /// Opens the ledger over an already-open database.
    ///
    /// Creates the table if it is absent, so that later reads — which run in read
    /// transactions and cannot create anything — always find it.
    pub fn new(db: Arc<Db>) -> Result<Self> {
        create_tables(db.database())?;
        Ok(Self { db })
    }
}

#[async_trait]
impl UsageLedger for RedbUsageLedger {
    async fn read(&self, subject: &str, window: &str) -> Result<Counters> {
        let db = self.db.clone();
        let subject = subject.to_owned();
        let window = window.to_owned();
        let joined = tokio::task::spawn_blocking(move || {
            let transaction = db
                .database()
                .begin_read()
                .map_err(|error| store_error("beginning a quota read", error))?;
            let table = open_read(&transaction)?;
            read_blocking(&table, &subject, &window)
        })
        .await;
        finish(joined, "reading a usage counter")
    }

    async fn add(
        &self,
        subject: &str,
        period: QuotaPeriod,
        window: &str,
        delta: Counters,
    ) -> Result<()> {
        let db = self.db.clone();
        let subject = subject.to_owned();
        let window = window.to_owned();
        let _queued = self.db.writes().lock().await;
        let joined = tokio::task::spawn_blocking(move || {
            let transaction = db
                .database()
                .begin_write()
                .map_err(|error| store_error("beginning a quota write", error))?;
            {
                let mut table = transaction
                    .open_table(USAGE)
                    .map_err(|error| store_error("opening the quota usage table", error))?;

                let current = table
                    .get((subject.as_str(), window.as_str()))
                    .map_err(|error| store_error("reading a usage counter", error))?
                    .map(|value| decode(value.value()))
                    .transpose()?
                    .unwrap_or_default();
                let encoded = encode(&current.saturating_add(delta))?;
                table
                    .insert((subject.as_str(), window.as_str()), encoded.as_slice())
                    .map_err(|error| store_error("writing a usage counter", error))?;

                // Everything under this subject and this period that sorts before the window
                // just written: those windows have closed, and closing is the only way a key
                // gets there — keys are chronological within a prefix. Collected first
                // because a range borrows the table that the removals mutate.
                let closed: Vec<(String, String)> = table
                    .range((subject.as_str(), period.prefix())..(subject.as_str(), window.as_str()))
                    .map_err(|error| store_error("scanning closed quota windows", error))?
                    .map(|entry| {
                        entry
                            .map(|(key, _)| {
                                let (subject, window) = key.value();
                                (subject.to_owned(), window.to_owned())
                            })
                            .map_err(|error| store_error("reading a closed quota window", error))
                    })
                    .collect::<Result<_>>()?;
                for (subject, window) in closed {
                    table
                        .remove((subject.as_str(), window.as_str()))
                        .map_err(|error| store_error("dropping a closed quota window", error))?;
                }
            }
            transaction
                .commit()
                .map_err(|error| store_error("committing a quota write", error))
        })
        .await;
        finish(joined, "recording usage")
    }
}

fn read_blocking(
    table: &ReadOnlyTable<(&'static str, &'static str), &'static [u8]>,
    subject: &str,
    window: &str,
) -> Result<Counters> {
    match table
        .get((subject, window))
        .map_err(|error| store_error("reading a usage counter", error))?
    {
        Some(value) => decode(value.value()),
        None => Ok(Counters::default()),
    }
}

/// Creates the ledger's table if it does not exist yet.
fn create_tables(db: &Database) -> Result<()> {
    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning the quota schema transaction", error))?;
    {
        transaction
            .open_table(USAGE)
            .map_err(|error| store_error("opening the quota usage table", error))?;
    }
    transaction
        .commit()
        .map_err(|error| store_error("committing the quota schema", error))
}

fn open_read(
    transaction: &ReadTransaction,
) -> Result<ReadOnlyTable<(&'static str, &'static str), &'static [u8]>> {
    transaction
        .open_table(USAGE)
        .map_err(|error| store_error("opening the quota usage table", error))
}

fn encode(counters: &Counters) -> Result<Vec<u8>> {
    serde_json::to_vec(counters).map_err(|error| Error::wrap("encoding a usage counter", error))
}

/// Decodes a stored row.
///
/// A failure means the bytes on disk are not what this build writes. Reported rather than
/// treated as zero: a ledger that answers "nothing spent" whenever it cannot read itself is
/// one that grants an unbounded budget to whatever corrupted it.
fn decode(bytes: &[u8]) -> Result<Counters> {
    serde_json::from_slice(bytes).map_err(|error| Error::wrap("decoding a usage counter", error))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (tempfile::TempDir, RedbUsageLedger) {
        let directory = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(directory.path().join("aik.redb")).unwrap());
        let ledger = RedbUsageLedger::new(db).unwrap();
        (directory, ledger)
    }

    fn turns(count: u64) -> Counters {
        Counters {
            turns: count,
            ..Counters::default()
        }
    }

    #[tokio::test]
    async fn an_unwritten_row_reads_as_nothing_spent() {
        let (_directory, ledger) = ledger();
        assert!(
            ledger
                .read("alice", "day:2026-08-28")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn adds_accumulate_and_survive_reopening_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aik.redb");
        {
            let db = Arc::new(Db::open(&path).unwrap());
            let ledger = RedbUsageLedger::new(db).unwrap();
            for _ in 0..3 {
                ledger
                    .add("alice", QuotaPeriod::Day, "day:2026-08-28", turns(1))
                    .await
                    .unwrap();
            }
        }

        let db = Arc::new(Db::open(&path).unwrap());
        let ledger = RedbUsageLedger::new(db).unwrap();
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            3,
            "restarting must not reset a budget"
        );
    }

    #[tokio::test]
    async fn a_write_drops_this_subjects_closed_windows_and_nothing_else() {
        let (_directory, ledger) = ledger();
        ledger
            .add("alice", QuotaPeriod::Day, "day:2026-08-26", turns(1))
            .await
            .unwrap();
        ledger
            .add("alice", QuotaPeriod::Day, "day:2026-08-27", turns(1))
            .await
            .unwrap();
        ledger
            .add("alice", QuotaPeriod::Month, "month:2026-08", turns(2))
            .await
            .unwrap();
        ledger
            .add("bob", QuotaPeriod::Day, "day:2026-08-27", turns(4))
            .await
            .unwrap();

        ledger
            .add("alice", QuotaPeriod::Day, "day:2026-08-28", turns(1))
            .await
            .unwrap();

        assert!(
            ledger
                .read("alice", "day:2026-08-26")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            ledger
                .read("alice", "day:2026-08-27")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );
        assert_eq!(
            ledger.read("alice", "month:2026-08").await.unwrap().turns,
            2,
            "a daily write must not reclaim the monthly counter it sits inside"
        );
        assert_eq!(
            ledger.read("bob", "day:2026-08-27").await.unwrap().turns,
            4,
            "one subject's write must not touch another's rows"
        );
    }

    #[tokio::test]
    async fn concurrent_adds_do_not_lose_updates() {
        let (_directory, ledger) = ledger();
        let ledger = Arc::new(ledger);
        let mut handles = Vec::new();
        for _ in 0..16 {
            let ledger = ledger.clone();
            handles.push(tokio::spawn(async move {
                ledger
                    .add("alice", QuotaPeriod::Day, "day:2026-08-28", turns(1))
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            16,
            "a quota enforced by a lost update is not enforced"
        );
    }

    #[tokio::test]
    async fn a_corrupt_row_is_an_error_rather_than_an_empty_budget() {
        let (_directory, ledger) = ledger();
        {
            let transaction = ledger.db.database().begin_write().unwrap();
            {
                let mut table = transaction.open_table(USAGE).unwrap();
                table
                    .insert(("alice", "day:2026-08-28"), b"not json".as_slice())
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        assert!(ledger.read("alice", "day:2026-08-28").await.is_err());
    }
}
