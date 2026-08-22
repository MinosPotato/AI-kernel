//! What the durable store adds over the in-memory one, and what it must not lose while
//! adding it.
//!
//! The conformance suite in `behavior.rs` and the rules in `security.rs` already run against
//! both backends. These are the assertions that only mean something on disk: that a restart
//! changes nothing, that the indexes come back in step with the records they describe, that a
//! sweep large enough to need several transactions still finishes, and that a database
//! somebody has edited underneath us is reported rather than read as if it were merely empty.

use std::sync::Arc;

use aik_api::audit::{AuditEntry, AuditEntryKind, AuditQuery, AuditStore};
use aik_audit::{AuditRetentionSweeper, RedbAuditStore};
use aik_core::ErrorKind;
use aik_core::clock::{ManualClock, Timestamp};
use aik_store::Db;
use aik_store::redb::{ReadableDatabase, TableDefinition};

mod support;
use support::{Backend, allowed, invoked, user};

/// The record table, as `persistent.rs` defines it. Redeclared here rather than exported,
/// because the layout is the store's own business — a test that reaches into it is
/// deliberately going behind the API, and should look like it.
const RECORDS: TableDefinition<'static, u64, &[u8]> = TableDefinition::new("audit.records");

/// The time index, likewise.
const BY_TIME: TableDefinition<'static, (u64, u64), ()> = TableDefinition::new("audit.by_time");

/// The principal index, likewise.
const BY_PRINCIPAL: TableDefinition<'static, (&str, u64), ()> =
    TableDefinition::new("audit.by_principal");

/// The correlation index, likewise.
const BY_CORRELATION: TableDefinition<'static, (u128, u64), ()> =
    TableDefinition::new("audit.by_correlation");

#[tokio::test]
async fn a_restart_changes_nothing_about_what_was_recorded() {
    let mut fixture = Backend::Redb.open();

    for at in 0..5 {
        fixture
            .store()
            .append(allowed("alice", "fs.read", at))
            .await
            .unwrap();
    }
    let before = fixture
        .store()
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();

    fixture.reopen();

    let after = fixture
        .store()
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(fixture.store().last_sequence().await.unwrap(), 5);
}

#[tokio::test]
async fn sequence_numbers_continue_across_a_restart() {
    let mut fixture = Backend::Redb.open();

    fixture
        .store()
        .append(allowed("alice", "fs.read", 1))
        .await
        .unwrap();
    fixture.reopen();

    let next = fixture
        .store()
        .append(invoked("alice", "fs.read", 2))
        .await
        .unwrap();
    assert_eq!(
        next, 2,
        "a restart must not restart the numbering; a repeated sequence number would make two \
         different records indistinguishable"
    );
}

#[tokio::test]
async fn a_sweep_that_emptied_the_table_still_does_not_reuse_a_number_after_a_restart() {
    // The failure this excludes is the one a naive "largest key plus one" counter has: sweep
    // everything, restart, and the trail silently begins again at 1.
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(10_000));

    for at in [10, 20, 30] {
        fixture
            .store()
            .append(allowed("alice", "fs.read", at))
            .await
            .unwrap();
    }
    assert_eq!(fixture.sweep(Timestamp::from_millis(100)).await, 3);
    fixture.reopen();

    let next = fixture
        .store()
        .append(allowed("alice", "fs.read", 200))
        .await
        .unwrap();
    assert_eq!(next, 5, "3 records, then the marker at 4, then this");
}

#[tokio::test]
async fn every_index_comes_back_in_step_with_the_records_it_describes() {
    let fixture = Backend::Redb.open();
    let store = fixture.store();

    // One delegated record, which is indexed under two principals, and one that is not.
    store
        .append(support::invocation(
            "assistant",
            Some("alice"),
            "fs.write",
            10,
            aik_core::id::CorrelationId::new(),
            aik_api::audit::InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();
    store.append(allowed("alice", "fs.read", 20)).await.unwrap();
    drop(store);

    let mut fixture = fixture;
    fixture.close();

    let db = Db::open(fixture.path()).expect("the database reopens");
    let transaction = db.database().begin_read().unwrap();
    let records = transaction.open_table(RECORDS).unwrap();
    let by_time = transaction.open_table(BY_TIME).unwrap();
    let by_principal = transaction.open_table(BY_PRINCIPAL).unwrap();
    let by_correlation = transaction.open_table(BY_CORRELATION).unwrap();

    use aik_store::redb::ReadableTableMetadata;
    assert_eq!(records.len().unwrap(), 2);
    assert_eq!(
        by_time.len().unwrap(),
        2,
        "one time-index entry per record, no more and no fewer"
    );
    assert_eq!(
        by_correlation.len().unwrap(),
        2,
        "one correlation-index entry per record, including records with no correlation"
    );
    assert_eq!(
        by_principal.len().unwrap(),
        3,
        "the delegated record is indexed under both the actor and whoever it acted for"
    );
}

#[tokio::test]
async fn a_sweep_removes_every_index_entry_it_removed_a_record_for() {
    let fixture = Backend::Redb.at(Timestamp::from_millis(10_000));
    fixture
        .store()
        .append(support::invocation(
            "assistant",
            Some("alice"),
            "fs.write",
            10,
            aik_core::id::CorrelationId::new(),
            aik_api::audit::InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();
    assert_eq!(fixture.sweep(Timestamp::from_millis(100)).await, 1);

    let mut fixture = fixture;
    fixture.close();

    let db = Db::open(fixture.path()).expect("the database reopens");
    let transaction = db.database().begin_read().unwrap();
    use aik_store::redb::ReadableTableMetadata;
    // Only the retention marker is left, and it is indexed exactly once everywhere.
    assert_eq!(transaction.open_table(RECORDS).unwrap().len().unwrap(), 1);
    assert_eq!(transaction.open_table(BY_TIME).unwrap().len().unwrap(), 1);
    assert_eq!(
        transaction.open_table(BY_PRINCIPAL).unwrap().len().unwrap(),
        1,
        "a swept record must not leave either of its principal-index entries behind"
    );
    assert_eq!(
        transaction
            .open_table(BY_CORRELATION)
            .unwrap()
            .len()
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn a_sweep_larger_than_one_batch_still_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(10_000)));
    let db = Arc::new(Db::open(&path).unwrap());
    let store = Arc::new(
        RedbAuditStore::new(db)
            .unwrap()
            .with_clock(clock)
            .with_sweep_batch(4),
    );

    for at in 0..17 {
        store.append(allowed("alice", "fs.read", at)).await.unwrap();
    }

    let removed = store
        .sweep_older_than(Timestamp::from_millis(1_000))
        .await
        .unwrap();
    assert_eq!(removed, 17, "the sweep loops until nothing is due");

    let left = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert!(
        left.iter()
            .all(|record| record.entry.kind() == AuditEntryKind::Retention),
        "nothing but the markers accounting for the sweep is left"
    );
    assert!(
        !left.is_empty(),
        "a batched sweep still accounts for what it removed"
    );
    let accounted: u64 = left
        .iter()
        .map(|record| match &record.entry {
            AuditEntry::Retention(applied) => applied.removed,
            other => panic!("unexpected entry {other:?}"),
        })
        .sum();
    assert_eq!(
        accounted, 17,
        "the markers together account for every record removed"
    );
}

#[tokio::test]
async fn a_record_that_cannot_be_decoded_is_reported_rather_than_skipped() {
    let mut fixture = Backend::Redb.open();
    fixture
        .store()
        .append(allowed("alice", "fs.read", 10))
        .await
        .unwrap();
    fixture.close();

    // Somebody — or something — has been in the file. An audit store that answered "no
    // records" here would be indistinguishable from one that had never been written to.
    {
        let db = Db::open(fixture.path()).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            records.insert(1, b"not json".as_slice()).unwrap();
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();
    let error = fixture
        .store()
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("decoding"), "{error}");
}

#[tokio::test]
async fn an_index_entry_with_no_record_is_reported_as_the_corruption_it_is() {
    let mut fixture = Backend::Redb.open();
    fixture
        .store()
        .append(allowed("alice", "fs.read", 10))
        .await
        .unwrap();
    fixture.close();

    {
        let db = Db::open(fixture.path()).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            records.remove(1).unwrap();
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();
    let error = fixture
        .store()
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no record is stored"), "{error}");
}

#[tokio::test]
async fn the_trail_shares_one_database_with_the_other_durable_subsystems() {
    // The audit tables are namespaced under `audit.`, so they coexist with `mem.`,
    // `context.` and `sched.` in one file. This is what makes one backup, one path and one
    // schema version enough for the whole system.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let db = Arc::new(Db::open(&path).unwrap());

    let audit = RedbAuditStore::new(db.clone()).unwrap();
    audit.append(allowed("alice", "fs.read", 10)).await.unwrap();

    assert_eq!(db.schema_version().unwrap(), aik_store::SCHEMA_VERSION);
    assert_eq!(audit.last_sequence().await.unwrap(), 1);
}

#[tokio::test]
async fn a_database_written_by_a_newer_build_is_refused_before_anything_is_audited() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut meta = transaction
                .open_table(TableDefinition::<&str, u32>::new("aik.meta"))
                .unwrap();
            meta.insert("schema_version", aik_store::SCHEMA_VERSION + 1)
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    let error = Db::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
}
