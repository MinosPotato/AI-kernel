//! What the persistent store adds over the in-memory one, and what it must not lose while
//! adding it.
//!
//! The behavioural suites in `budgeting.rs` and `isolation.rs` already run against both
//! implementations. These are the assertions that only mean something on disk: that a
//! restart changes nothing, that a session boundary is a boundary in the file and not only
//! in a lock, and that a database somebody has edited underneath us is reported rather than
//! read as if it were merely empty.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::ContextBudget;
use aik_api::permission::PrincipalId;
use aik_context::DEFAULT_MAX_RECORDS_PER_SESSION;
use aik_core::ErrorKind;
use aik_store::Db;
use aik_store::redb::TableDefinition;

mod support;
use support::{Backend, open_redb, say, user};

/// The record table, as `persistent.rs` defines it. Redeclared here rather than exported,
/// because the layout is the store's own business — a test that reaches into it is
/// deliberately going behind the API, and should look like it.
const RECORDS: TableDefinition<'static, (u128, u64), &[u8]> =
    TableDefinition::new("context.records");

#[tokio::test]
async fn a_transcript_survives_a_restart_intact() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();
    let cx = user("alice");

    let first = fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    let second = fixture
        .store()
        .append(&session, say("two"), &cx)
        .await
        .unwrap();
    let before = fixture.store().stats(&session, &cx).await.unwrap().unwrap();

    fixture.reopen();
    let store = fixture.store();

    // Every field, not merely the message: a record whose sequence, attribution or cost
    // changed across a restart would still read back as the same conversation, and the
    // damage would only show up in a budget or an audit much later.
    assert_eq!(
        store.get(&session, &first.id, &cx).await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        store.get(&session, &second.id, &cx).await.unwrap(),
        Some(second.clone())
    );
    assert_eq!(store.stats(&session, &cx).await.unwrap(), Some(before));

    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    assert_eq!(window.usage.included_records, 2);
    assert_eq!(window.messages, vec![first.message, second.message]);
}

#[tokio::test]
async fn sequencing_continues_where_the_previous_process_left_off() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();
    let cx = user("alice");

    fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    fixture
        .store()
        .append(&session, say("two"), &cx)
        .await
        .unwrap();

    fixture.reopen();
    let third = fixture
        .store()
        .append(&session, say("three"), &cx)
        .await
        .unwrap();

    // A restart that restarted the numbering would silently overwrite the transcript it was
    // supposed to be extending.
    assert_eq!(third.sequence, 2);
    let window = fixture
        .store()
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    assert_eq!(window.usage.included_records, 3);
}

#[tokio::test]
async fn ownership_survives_a_restart() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();

    fixture
        .store()
        .append(&session, say("one"), &user("alice"))
        .await
        .unwrap();

    fixture.reopen();
    let store = fixture.store();

    let error = store
        .window(&session, &ContextBudget::UNLIMITED, &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);

    let stats = store
        .stats(&session, &user("alice"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.owner, PrincipalId::new("alice"));
}

#[tokio::test]
async fn sessions_stay_separate_in_the_file() {
    let mut fixture = Backend::Redb.open();
    let alice = SessionId::new();
    let bob = SessionId::new();

    let hers = fixture
        .store()
        .append(&alice, say("hers"), &user("alice"))
        .await
        .unwrap();
    let his = fixture
        .store()
        .append(&bob, say("his"), &user("bob"))
        .await
        .unwrap();

    fixture.reopen();
    let store = fixture.store();

    // Both transcripts are rows in one table. Nothing but the key layout keeps a scan of
    // one session away from the other's rows, so this is the assertion that the layout is
    // doing its job.
    assert!(
        store
            .get(&alice, &his.id, &user("alice"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get(&bob, &hers.id, &user("bob"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .window(&alice, &ContextBudget::UNLIMITED, &user("alice"))
            .await
            .unwrap()
            .messages,
        vec![hers.message]
    );
}

#[tokio::test]
async fn clearing_reaches_the_file_not_just_the_process() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();
    let cx = user("alice");

    let record = fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    assert_eq!(fixture.store().clear(&session, &cx).await.unwrap(), 1);

    fixture.reopen();
    let store = fixture.store();

    // A `clear` that only forgot in memory would be worse than none: the caller was told
    // the transcript was gone, and a restart would produce it again.
    assert!(store.stats(&session, &cx).await.unwrap().is_none());
    assert!(
        store
            .get(&session, &record.id, &cx)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_refused_append_leaves_nothing_on_disk() {
    let mut fixture = Backend::Redb.bounded(1);
    let session = SessionId::new();
    let cx = user("alice");

    fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    fixture
        .store()
        .append(&session, say("two"), &cx)
        .await
        .unwrap_err();

    fixture.reopen();
    let store = fixture.store();

    // The refusal happens partway through a transaction that has already written the record
    // row. Aborting has to take the header and the index with it, or the restart would find
    // a session whose totals disagree with its rows.
    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.records, 1);
    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    assert_eq!(window.usage.included_records, 1);
    assert_eq!(window.usage.total_tokens(), stats.tokens);
}

#[tokio::test]
async fn concurrent_appends_to_separate_sessions_do_not_interleave() {
    let fixture = Backend::Redb.open();
    let store = fixture.store();
    let sessions: Vec<SessionId> = (0..4).map(|_| SessionId::new()).collect();

    let mut writers = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        for turn in 0..8 {
            let store = Arc::clone(&store);
            let session = *session;
            writers.push(tokio::spawn(async move {
                store
                    .append(
                        &session,
                        say(&format!("session {index} turn {turn}")),
                        &user(&format!("owner-{index}")),
                    )
                    .await
                    .unwrap();
            }));
        }
    }
    for writer in writers {
        writer.await.unwrap();
    }

    for (index, session) in sessions.iter().enumerate() {
        let cx = user(&format!("owner-{index}"));
        let stats = store.stats(session, &cx).await.unwrap().unwrap();
        assert_eq!(stats.records, 8, "session {index} lost or gained records");

        let window = store
            .window(session, &ContextBudget::UNLIMITED, &cx)
            .await
            .unwrap();
        assert_eq!(window.usage.included_records, 8);
        assert_eq!(window.usage.total_tokens(), stats.tokens);
        for message in &window.messages {
            let text = serde_json::to_string(message).unwrap();
            assert!(
                text.contains(&format!("session {index} turn")),
                "session {index} holds another session's record: {text}"
            );
        }
    }
}

#[tokio::test]
async fn a_record_the_index_names_but_the_transcript_lacks_is_an_error() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();
    let cx = user("alice");

    let record = fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    let path = fixture.path().to_path_buf();

    // Delete the row from under the store, leaving the index pointing at it: the shape a
    // truncation, a bad restore or a deliberate edit would leave behind.
    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            let key = (session.as_uuid().as_u128(), 0u64);
            assert!(records.remove(key).unwrap().is_some(), "the row was there");
        }
        transaction.commit().unwrap();
    }

    let store = open_redb(&path, DEFAULT_MAX_RECORDS_PER_SESSION);
    let error = store.get(&session, &record.id, &cx).await.unwrap_err();

    // `Ok(None)` would read as "no such record", which is a lie that hides the tampering.
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(
        error.to_string().contains(&record.id.to_string()),
        "the error should name the record it could not find: {error}"
    );
}

#[tokio::test]
async fn an_unreadable_transcript_is_not_an_empty_one() {
    let mut fixture = Backend::Redb.open();
    let session = SessionId::new();
    let cx = user("alice");

    fixture
        .store()
        .append(&session, say("one"), &cx)
        .await
        .unwrap();
    let path = fixture.path().to_path_buf();

    fixture.close();
    {
        let db = Db::open(&path).unwrap();
        let transaction = db.database().begin_write().unwrap();
        {
            let mut records = transaction.open_table(RECORDS).unwrap();
            records
                .insert((session.as_uuid().as_u128(), 0u64), b"not json".as_slice())
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    let store = open_redb(&path, DEFAULT_MAX_RECORDS_PER_SESSION);
    let error = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);

    // A window assembled from what could still be parsed would hand the model a transcript
    // with a hole in it and no way to tell.
    assert!(
        store.stats(&session, &cx).await.is_ok(),
        "the header is intact, so stats should still answer"
    );
}

#[tokio::test]
async fn the_database_is_locked_while_a_store_holds_it() {
    let fixture = Backend::Redb.open();
    let _store = fixture.store();

    // Two processes writing one transcript through separate handles is the failure this
    // prevents; redb enforces it, and the store depends on that rather than re-checking.
    let error = Db::open(fixture.path()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
}

/// The owner index, as `persistent.rs` defines it. Redeclared for the same reason
/// [`RECORDS`] is: a test that inspects the layout is going behind the API deliberately.
const BY_OWNER: TableDefinition<'static, (&str, u128), ()> =
    TableDefinition::new("context.by_owner");

/// The idle-time index, likewise.
const BY_UPDATED: TableDefinition<'static, (u64, u128), ()> =
    TableDefinition::new("context.by_updated");

/// How many rows a table holds, over a database the test has opened itself.
fn count<K, V>(path: &std::path::Path, table: TableDefinition<'static, K, V>) -> usize
where
    K: aik_store::redb::Key + 'static,
    V: aik_store::redb::Value + 'static,
{
    use aik_store::redb::{ReadableDatabase, ReadableTable};
    let db = Db::open(path).expect("the database opens");
    let transaction = db.database().begin_read().expect("a read transaction");
    let table = transaction.open_table(table).expect("the table exists");
    table.iter().expect("a scan").count()
}

#[tokio::test]
async fn an_enumeration_survives_a_restart() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let (hers, his) = (SessionId::new(), SessionId::new());

    fixture
        .store()
        .append(&hers, say("mine"), &cx)
        .await
        .unwrap();
    fixture
        .store()
        .append(&his, say("bob's"), &user("bob"))
        .await
        .unwrap();
    let before = fixture.store().sessions(&cx).await.unwrap();

    fixture.reopen();

    // Identical, owner filtering included: the index is on disk, so the filtering it enables
    // is a property of the file rather than of a handle that happened to be open.
    assert_eq!(fixture.store().sessions(&cx).await.unwrap(), before);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].session, hers);

    let bob = fixture.store().sessions(&user("bob")).await.unwrap();
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].session, his);
}

#[tokio::test]
async fn compaction_reaches_the_file_not_just_the_process() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let session = SessionId::new();

    let mut ids = Vec::new();
    for body in ["one", "two", "three", "four"] {
        ids.push(
            fixture
                .store()
                .append(&session, say(body), &cx)
                .await
                .unwrap(),
        );
    }
    assert_eq!(fixture.store().compact(&session, 1, &cx).await.unwrap(), 3);
    let after = fixture.store().stats(&session, &cx).await.unwrap().unwrap();

    fixture.reopen();

    assert_eq!(
        fixture.store().stats(&session, &cx).await.unwrap().unwrap(),
        after
    );
    // The compacted records are gone from the record table *and* from the id index; a
    // surviving index entry would make `get` report corruption on a record that was correctly
    // removed.
    fixture.close();
    assert_eq!(count(fixture.path(), RECORDS), 1);
    fixture.reopen();
    for removed in &ids[..3] {
        assert!(
            fixture
                .store()
                .get(&session, &removed.id, &cx)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        fixture
            .store()
            .get(&session, &ids[3].id, &cx)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_cleared_session_leaves_no_index_entry_behind() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let (kept, cleared) = (SessionId::new(), SessionId::new());

    fixture
        .store()
        .append(&kept, say("keep"), &cx)
        .await
        .unwrap();
    fixture
        .store()
        .append(&cleared, say("go"), &cx)
        .await
        .unwrap();
    assert_eq!(fixture.store().clear(&cleared, &cx).await.unwrap(), 1);

    fixture.reopen();

    // Not merely absent from a listing — absent from the tables the listing and the sweep
    // read. A leftover entry would resurrect the session as an unreadable row that every
    // future enumeration reports as corruption.
    let listed = fixture.store().sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, kept);

    fixture.close();
    assert_eq!(count(fixture.path(), BY_OWNER), 1);
    assert_eq!(count(fixture.path(), BY_UPDATED), 1);
}

#[tokio::test]
async fn appending_leaves_exactly_one_idle_time_entry_per_session() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three", "four", "five"] {
        fixture
            .store()
            .append(&session, say(body), &cx)
            .await
            .unwrap();
    }

    fixture.close();
    // Every append moves the entry rather than adding one. A store that only inserted would
    // leave the session listed under an idle time it no longer has — and the oldest of those
    // is the one a retention sweep would find first.
    assert_eq!(count(fixture.path(), BY_UPDATED), 1);
    assert_eq!(count(fixture.path(), BY_OWNER), 1);
}

#[tokio::test]
async fn a_database_written_before_the_indexes_existed_is_reconciled_on_open() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let session = SessionId::new();
    fixture
        .store()
        .append(&session, say("hello"), &cx)
        .await
        .unwrap();
    fixture.close();

    // What an older build's file looks like: the headers and records are all there, and
    // neither index table has anything in it. Emptying them by hand is the closest a test can
    // get to a database this build never wrote.
    {
        let db = Db::open(fixture.path()).expect("the database opens");
        let transaction = db.database().begin_write().expect("a write transaction");
        {
            let mut by_owner = transaction.open_table(BY_OWNER).expect("the owner index");
            by_owner.retain(|_, ()| false).expect("emptied");
            let mut by_updated = transaction.open_table(BY_UPDATED).expect("the idle index");
            by_updated.retain(|_, ()| false).expect("emptied");
        }
        transaction.commit().expect("committed");
    }
    assert_eq!(count(fixture.path(), BY_OWNER), 0);

    fixture.reopen();

    // Opening the store is the whole of the upgrade: the session is enumerable again, and
    // the owner it is listed under came from its header rather than from whoever is asking.
    let listed = fixture.store().sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, session);
    assert_eq!(listed[0].owner, PrincipalId::new("alice"));
    assert!(
        fixture
            .store()
            .sessions(&user("mallory"))
            .await
            .unwrap()
            .is_empty(),
        "reconciliation restores the recorded ownership; it does not widen it"
    );
}

#[tokio::test]
async fn an_index_entry_naming_a_session_that_is_gone_is_reconciled_away() {
    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let session = SessionId::new();
    fixture
        .store()
        .append(&session, say("hello"), &cx)
        .await
        .unwrap();
    fixture.close();

    // A dangling entry, of the kind a partial write by something that is not this store could
    // leave. Enumeration treats it as corruption, so reconciliation has to remove it rather
    // than leave every future listing failing.
    {
        let db = Db::open(fixture.path()).expect("the database opens");
        let transaction = db.database().begin_write().expect("a write transaction");
        {
            let mut by_owner = transaction.open_table(BY_OWNER).expect("the owner index");
            by_owner
                .insert(("alice", 0xdead_beef_u128), ())
                .expect("a dangling entry");
        }
        transaction.commit().expect("committed");
    }
    assert_eq!(count(fixture.path(), BY_OWNER), 2);

    fixture.reopen();

    let listed = fixture.store().sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, session);
    fixture.close();
    assert_eq!(count(fixture.path(), BY_OWNER), 1);
}

/// The store's own bookkeeping table, as `aik-store` defines it. Redeclared because the
/// definition is `pub(crate)` there — a test that stamps a version by hand is going behind
/// the API on purpose, and should look like it.
const META: TableDefinition<'static, &str, u32> = TableDefinition::new("aik.meta");

#[tokio::test]
async fn a_database_at_the_previous_schema_version_is_upgraded_rather_than_refused() {
    use aik_store::redb::Database;

    let mut fixture = Backend::Redb.open();
    let cx = user("alice");
    let session = SessionId::new();
    fixture
        .store()
        .append(&session, say("written by the last release"), &cx)
        .await
        .unwrap();
    fixture.close();

    // Stamped back to the version before these tables existed, which is what a file written
    // by the previous build carries. Written through raw redb rather than through `Db`,
    // because `Db::open` is the thing under test and would migrate it on the way in.
    {
        let db = Database::create(fixture.path()).expect("the database opens");
        let transaction = db.begin_write().expect("a write transaction");
        {
            let mut meta = transaction.open_table(META).expect("the meta table");
            meta.insert("schema_version", aik_store::SCHEMA_VERSION - 1)
                .expect("stamped");
        }
        transaction.commit().expect("committed");
    }

    // Opening runs the migration policy: older is upgraded, not refused. The bump introduced
    // only tables, so there is nothing to transform — the store's own reconciliation is what
    // fills them, and the conversation is intact and enumerable afterwards.
    // `reopen` goes through `Db::open`, which is where the version decision is made: a file
    // this build refused would panic here rather than reaching the assertions below.
    fixture.reopen();

    let listed = fixture.store().sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, session);
    assert_eq!(listed[0].records, 1);
    assert_eq!(listed[0].owner, PrincipalId::new("alice"));

    fixture.close();
    let db = Db::open(fixture.path()).expect("the database opens");
    assert_eq!(
        db.schema_version().unwrap(),
        aik_store::SCHEMA_VERSION,
        "and the file now records the version of the build that has it"
    );
}
