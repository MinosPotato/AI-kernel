//! What the persistent store adds over the in-memory one, and what it must not lose while
//! adding it.
//!
//! The behavioural suite in `behavior.rs` already runs against both implementations. These
//! are the assertions that only mean something on disk: that a restart changes nothing, that
//! both indexes come back in step with the records they describe, and that a database
//! somebody has edited underneath us is reported rather than read as if it were merely empty.
//!
//! This mirrors `aik-context`'s `persistence.rs` deliberately. The two durable stores share
//! one file and one set of failure modes, and a gap in one suite is a gap in the guarantee
//! both of them make.

use std::sync::Arc;

use aik_api::memory::{MemoryId, MemoryKind, MemoryQuery, MemoryRecord, MemoryStore};
use aik_api::permission::PrincipalId;
use aik_core::Clock;
use aik_core::ErrorKind;
use aik_core::clock::{ManualClock, Timestamp};
use aik_store::Db;
use aik_store::redb::TableDefinition;
use serde_json::json;

mod support;
use support::{Backend, user};

/// The record table, as `persistent.rs` defines it. Redeclared here rather than exported,
/// because the layout is the store's own business — a test that reaches into it is
/// deliberately going behind the API, and should look like it.
const RECORDS: TableDefinition<'static, u128, &[u8]> = TableDefinition::new("mem.records");

/// The kind index, likewise.
const BY_KIND: TableDefinition<'static, (&str, u128), ()> = TableDefinition::new("mem.by_kind");

/// The expiry index, likewise.
const BY_EXPIRY: TableDefinition<'static, (u64, u128), ()> = TableDefinition::new("mem.by_expiry");

/// The owner index, likewise.
const BY_OWNER: TableDefinition<'static, (&str, u128), ()> = TableDefinition::new("mem.by_owner");

fn record(kind: &str, created_at_ms: u64) -> MemoryRecord {
    MemoryRecord::new(
        kind,
        json!({ "n": created_at_ms }),
        Timestamp::from_millis(created_at_ms),
    )
}

#[tokio::test]
async fn a_record_survives_a_restart_intact() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut stored = record("preference", 1);
    stored.metadata.insert("source".into(), json!("chat"));
    stored.expires_at = Some(Timestamp::from_millis(9_000));
    stored.embedding = Some(vec![0.25, -0.5]);
    fixture.store().put(stored.clone(), &cx).await.unwrap();

    fixture.reopen();

    // Every field, not merely the content: a record whose kind, metadata, expiry or
    // embedding changed across a restart would still read back as the same memory, and the
    // damage would only show up in a query or a sweep much later.
    assert_eq!(
        fixture.store().get(&stored.id, &cx).await.unwrap(),
        Some(MemoryRecord {
            owner: PrincipalId::new("alice"),
            ..stored
        })
    );
}

#[tokio::test]
async fn both_indexes_survive_a_restart() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut expiring = record("fact", 1);
    expiring.expires_at = Some(Timestamp::from_millis(1_500));
    let permanent = record("preference", 2);
    fixture.store().put(expiring.clone(), &cx).await.unwrap();
    fixture.store().put(permanent.clone(), &cx).await.unwrap();

    fixture.reopen();

    // An index rebuilt only in memory would come back empty and turn every kind-filtered
    // query into a silent miss.
    let by_kind = MemoryQuery {
        kinds: vec![MemoryKind::new("preference")],
        ..Default::default()
    };
    let matches = fixture.store().query(&by_kind, &cx).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, permanent.id);

    // And an expiry index that did not survive would leave the record invisible to `query`
    // for ever while never being reclaimed: dead weight that no sweep can find.
    fixture.clock().set(Timestamp::from_millis(2_000));
    assert_eq!(fixture.sweep().await, 1);
    assert!(
        fixture
            .store()
            .get(&expiring.id, &cx)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .store()
            .get(&permanent.id, &cx)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn an_upsert_leaves_one_row_and_one_index_entry_after_a_restart() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut stored = record("draft", 1);
    stored.expires_at = Some(Timestamp::from_millis(1_500));
    fixture.store().put(stored.clone(), &cx).await.unwrap();

    stored.kind = MemoryKind::new("final");
    stored.expires_at = Some(Timestamp::from_millis(8_000));
    fixture.store().put(stored.clone(), &cx).await.unwrap();

    fixture.reopen();
    let path = fixture.path().to_path_buf();

    let matches = fixture
        .store()
        .query(&MemoryQuery::default(), &cx)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1, "an upsert must not leave a second row");

    // Counting the index rows directly, because a stale entry is exactly the kind of thing a
    // query cannot see: it points at a record that still exists, so nothing goes wrong until
    // a sweep acts on it.
    fixture.close();
    let db = Db::open(&path).unwrap();
    assert_eq!(rows_in(&db, BY_KIND), 1, "one kind index entry per record");
    assert_eq!(
        rows_in(&db, BY_EXPIRY),
        1,
        "the retracted expiry must not still be indexed"
    );
    assert_eq!(
        rows_in(&db, BY_OWNER),
        1,
        "a replacement keeps its owner, so the owner index must not gain a second entry"
    );
}

#[tokio::test]
async fn deleting_reaches_the_file_and_takes_both_indexes_with_it() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut stored = record("fact", 1);
    stored.expires_at = Some(Timestamp::from_millis(5_000));
    fixture.store().put(stored.clone(), &cx).await.unwrap();
    assert!(fixture.store().delete(&stored.id, &cx).await.unwrap());

    fixture.reopen();
    let path = fixture.path().to_path_buf();

    // A delete that only forgot in memory would be worse than none: the caller was told the
    // record was gone, and a restart would produce it again.
    assert!(
        fixture
            .store()
            .get(&stored.id, &cx)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .store()
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .is_empty()
    );

    fixture.close();
    let db = Db::open(&path).unwrap();
    assert_eq!(rows_in(&db, RECORDS), 0);
    assert_eq!(rows_in(&db, BY_KIND), 0, "the kind index entry went too");
    assert_eq!(
        rows_in(&db, BY_EXPIRY),
        0,
        "the expiry index entry went too"
    );
    assert_eq!(rows_in(&db, BY_OWNER), 0, "the owner index entry went too");
}

#[tokio::test]
async fn a_sweep_reaches_the_file_not_just_the_process() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut expired = record("fact", 1);
    expired.expires_at = Some(Timestamp::from_millis(1_500));
    fixture.store().put(expired.clone(), &cx).await.unwrap();

    fixture.clock().set(Timestamp::from_millis(2_000));
    assert_eq!(fixture.sweep().await, 1);

    fixture.reopen();
    let path = fixture.path().to_path_buf();

    assert!(
        fixture
            .store()
            .get(&expired.id, &cx)
            .await
            .unwrap()
            .is_none()
    );

    fixture.close();
    let db = Db::open(&path).unwrap();
    assert_eq!(rows_in(&db, RECORDS), 0);
    assert_eq!(rows_in(&db, BY_KIND), 0);
    assert_eq!(rows_in(&db, BY_EXPIRY), 0);
    assert_eq!(rows_in(&db, BY_OWNER), 0);
}

#[tokio::test]
async fn a_record_the_kind_index_names_but_the_table_lacks_is_an_error() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let stored = record("fact", 1);
    fixture.store().put(stored.clone(), &cx).await.unwrap();
    let path = fixture.path().to_path_buf();

    // Delete the row from under the store, leaving the index pointing at it: the shape a
    // truncation, a bad restore or a deliberate edit would leave behind.
    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            assert!(
                records
                    .remove(stored.id.as_uuid().as_u128())
                    .unwrap()
                    .is_some(),
                "the row was there"
            );
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();
    let query = MemoryQuery {
        kinds: vec![stored.kind.clone()],
        ..Default::default()
    };
    let error = fixture.store().query(&query, &cx).await.unwrap_err();

    // An empty result would read as "nothing of that kind", which is a lie that hides the
    // tampering.
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(
        error.to_string().contains(&stored.id.to_string()),
        "the error should name the record it could not find: {error}"
    );
}

#[tokio::test]
async fn a_record_the_owner_index_names_but_the_table_lacks_is_an_error() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let stored = record("fact", 1);
    fixture.store().put(stored.clone(), &cx).await.unwrap();
    let path = fixture.path().to_path_buf();

    // Same tampering as the kind-index case, but reached through the *other* query path:
    // `query_blocking` walks `mem.by_owner` rather than `mem.by_kind` when the query names
    // no kind, and that path has its own dangling-entry check that nothing else exercises.
    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            assert!(
                records
                    .remove(stored.id.as_uuid().as_u128())
                    .unwrap()
                    .is_some(),
                "the row was there"
            );
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();
    let error = fixture
        .store()
        .query(&MemoryQuery::default(), &cx)
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(
        error.to_string().contains(&stored.id.to_string()),
        "the error should name the record it could not find: {error}"
    );
}

#[tokio::test]
async fn a_record_the_expiry_index_names_but_the_table_lacks_is_an_error() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut stored = record("fact", 1);
    stored.expires_at = Some(Timestamp::from_millis(1_500));
    fixture.store().put(stored.clone(), &cx).await.unwrap();
    let path = fixture.path().to_path_buf();

    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            records
                .remove(stored.id.as_uuid().as_u128())
                .unwrap()
                .expect("the row was there");
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();
    fixture.clock().set(Timestamp::from_millis(2_000));

    // The sweep is housekeeping nobody is watching, so it is the one place a silent skip
    // would never be noticed. It has to complain.
    let error = fixture
        .sweeper()
        .sweep_expired(Timestamp::from_millis(2_000))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(
        error.to_string().contains(&stored.id.to_string()),
        "the error should name the record it could not find: {error}"
    );
}

#[tokio::test]
async fn an_unreadable_record_is_not_an_absent_one() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let stored = record("fact", 1);
    fixture.store().put(stored.clone(), &cx).await.unwrap();
    let path = fixture.path().to_path_buf();

    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            records
                .insert(stored.id.as_uuid().as_u128(), b"not json".as_slice())
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    fixture.reopen();

    // Reporting `Ok(None)` would let a caller act as though the memory had been forgotten
    // when it is in fact merely unreadable, and write a second one on top of it.
    let error = fixture.store().get(&stored.id, &cx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);

    let error = fixture
        .store()
        .query(&MemoryQuery::default(), &cx)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Other,
        "a query must not quietly skip the rows it could not parse"
    );
}

#[tokio::test]
async fn a_missing_id_is_still_none_after_a_restart() {
    let mut fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");
    fixture.store().put(record("fact", 1), &cx).await.unwrap();

    fixture.reopen();

    // The complement of the corruption tests above: an id that was never stored is a lookup
    // miss, not an error, however much else is in the file.
    assert!(
        fixture
            .store()
            .get(&MemoryId::new(), &cx)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!fixture.store().delete(&MemoryId::new(), &cx).await.unwrap());
}

#[tokio::test]
async fn a_backlog_larger_than_one_batch_is_reclaimed_in_full() {
    use aik_memory::{ExpirySweeper, RedbMemoryStore};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_000)));
    let cx = user("alice");

    // A batch far smaller than the backlog, so the sweep is forced to loop. The default is
    // large enough that reproducing this through it would mean thousands of commits.
    let batch = 4;
    let store = Arc::new(
        RedbMemoryStore::new(Arc::new(Db::open(&path).unwrap()))
            .unwrap()
            .with_clock(clock.clone())
            .with_sweep_batch(batch),
    );

    let expiring = batch * 3 + 1;
    for index in 0..expiring as u64 {
        let mut due = record("fact", index);
        // Spread the expiries so the index has many distinct keys rather than one crowded
        // one: a batch boundary that fell inside a single millisecond is the case a
        // fixed-size batch could otherwise loop on for ever.
        due.expires_at = Some(Timestamp::from_millis(1_100 + index));
        store.put(due, &cx).await.unwrap();
    }
    let permanent = record("preference", 1);
    store.put(permanent.clone(), &cx).await.unwrap();

    clock.set(Timestamp::from_millis(5_000));
    let removed = store.sweep_expired(clock.now()).await.unwrap();

    assert_eq!(
        removed, expiring,
        "the sweep stopped after a batch instead of looping until nothing was due"
    );
    assert_eq!(
        store
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(store.get(&permanent.id, &cx).await.unwrap().is_some());

    // And the indexes went with them, not just the rows.
    drop(store);
    let db = Db::open(&path).unwrap();
    assert_eq!(rows_in(&db, RECORDS), 1);
    assert_eq!(rows_in(&db, BY_KIND), 1);
    assert_eq!(rows_in(&db, BY_EXPIRY), 0);
    assert_eq!(
        rows_in(&db, BY_OWNER),
        1,
        "the survivor's owner entry, and no others"
    );
}

#[tokio::test]
async fn a_sweep_with_nothing_due_reclaims_nothing_and_is_repeatable() {
    let fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let cx = user("alice");

    let mut later = record("fact", 1);
    later.expires_at = Some(Timestamp::from_millis(9_000));
    fixture.store().put(later.clone(), &cx).await.unwrap();

    // An idle sweep is the overwhelmingly common case — once a minute, for ever — so it must
    // be cheap and it must not disturb anything.
    assert_eq!(fixture.sweep().await, 0);
    assert_eq!(fixture.sweep().await, 0);
    assert_eq!(
        fixture.store().get(&later.id, &cx).await.unwrap(),
        Some(MemoryRecord {
            owner: PrincipalId::new("alice"),
            ..later
        })
    );
}

#[tokio::test]
async fn the_database_is_locked_while_a_store_holds_it() {
    let fixture = Backend::Redb.at(Timestamp::from_millis(1_000));
    let _store = fixture.store();

    // Two processes writing one set of memories through separate handles is the failure this
    // prevents; redb enforces it, and the store depends on that rather than re-checking.
    let error = Db::open(fixture.path()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
}

/// How many rows a table holds, opened directly rather than through the store.
fn rows_in<V>(
    db: &Db,
    table: TableDefinition<'static, impl aik_store::redb::Key + 'static, V>,
) -> usize
where
    V: aik_store::redb::Value + 'static,
{
    use aik_store::redb::{ReadableDatabase, ReadableTableMetadata};

    let transaction = db.database().begin_read().unwrap();
    match transaction.open_table(table) {
        Ok(table) => table.len().unwrap() as usize,
        Err(aik_store::redb::TableError::TableDoesNotExist(_)) => 0,
        Err(error) => panic!("{error}"),
    }
}
