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
