//! The [`MemoryStore`] contract, run once against both implementations.
//!
//! A durable store that behaved subtly differently from the in-memory one — a different
//! upsert rule, a query that ranked differently, an expiry check only one of them applied —
//! would be a correctness regression delivered as a performance improvement. Writing every
//! assertion here once and running it against both backends is what keeps that impossible by
//! construction rather than by discipline.

use std::time::Duration;

use aik_api::memory::{MemoryId, MemoryQuery, MemoryRecord};
use aik_api::permission::PrincipalId;
use aik_core::ErrorKind;
use aik_core::clock::Timestamp;
use serde_json::json;

mod support;
use support::{Backend, user};

crate::both_backends!(
    a_missing_id_is_none,
    put_then_get_round_trips,
    put_upserts_by_id,
    replacing_a_record_moves_it_between_kinds,
    delete_reports_whether_the_record_existed,
    delete_removes_it_from_every_kind_filter,
    query_with_no_kinds_returns_every_kind,
    query_filters_by_kind,
    query_filters_by_metadata_exactly,
    query_respects_the_limit,
    query_ranks_most_recent_first,
    semantic_query_fields_are_all_unsupported,
    an_expired_record_is_not_query_visible,
    get_and_delete_ignore_expiry,
    a_record_without_expiry_is_never_swept,
    sweeping_reclaims_an_expired_record,
    changing_an_expiry_retires_the_old_one,
    clearing_an_expiry_makes_a_record_permanent,
    adding_an_expiry_to_a_permanent_record_takes_effect,
    concurrent_puts_are_all_visible,
);

fn record(kind: &str, created_at_ms: u64) -> MemoryRecord {
    MemoryRecord::new(
        kind,
        json!({"n": created_at_ms}),
        Timestamp::from_millis(created_at_ms),
    )
}

async fn a_missing_id_is_none(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    assert!(
        store
            .get(&MemoryId::new(), &user("alice"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .delete(&MemoryId::new(), &user("alice"))
            .await
            .unwrap()
    );
}

async fn put_then_get_round_trips(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let record = record("fact", 1);

    store.put(record.clone(), &cx).await.unwrap();

    // Everything round-trips unchanged except the owner, which the store assigns from the
    // context rather than accepting from the record it was handed.
    assert_ne!(
        record.owner,
        PrincipalId::new("alice"),
        "the fixture starts unowned, so the assertion below means something"
    );
    let expected = MemoryRecord {
        owner: PrincipalId::new("alice"),
        ..record.clone()
    };
    assert_eq!(store.get(&record.id, &cx).await.unwrap(), Some(expected));
}

async fn put_upserts_by_id(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let mut record = record("fact", 1);

    store.put(record.clone(), &cx).await.unwrap();
    record.content = json!({"n": 2});
    record.metadata.insert("revised".into(), json!(true));
    store.put(record.clone(), &cx).await.unwrap();

    let fetched = store.get(&record.id, &cx).await.unwrap().unwrap();
    assert_eq!(fetched.content, json!({"n": 2}));
    assert_eq!(fetched.metadata.get("revised"), Some(&json!(true)));

    let matches = store.query(&MemoryQuery::default(), &cx).await.unwrap();
    assert_eq!(
        matches.len(),
        1,
        "an upsert must not leave a second row behind"
    );
}

async fn replacing_a_record_moves_it_between_kinds(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let mut record = record("draft", 1);

    store.put(record.clone(), &cx).await.unwrap();
    record.kind = "final".into();
    store.put(record.clone(), &cx).await.unwrap();

    let draft_query = MemoryQuery {
        kinds: vec!["draft".into()],
        ..Default::default()
    };
    let final_query = MemoryQuery {
        kinds: vec!["final".into()],
        ..Default::default()
    };
    assert!(
        store.query(&draft_query, &cx).await.unwrap().is_empty(),
        "the old kind index entry must not survive a replacement"
    );
    let matches = store.query(&final_query, &cx).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, record.id);
}

async fn delete_reports_whether_the_record_existed(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let record = record("fact", 1);

    assert!(!store.delete(&record.id, &cx).await.unwrap());
    store.put(record.clone(), &cx).await.unwrap();
    assert!(store.delete(&record.id, &cx).await.unwrap());
    assert!(store.get(&record.id, &cx).await.unwrap().is_none());
    assert!(!store.delete(&record.id, &cx).await.unwrap());
}

async fn delete_removes_it_from_every_kind_filter(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let record = record("fact", 1);
    store.put(record.clone(), &cx).await.unwrap();

    store.delete(&record.id, &cx).await.unwrap();

    assert!(
        store
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .is_empty()
    );
    let by_kind = MemoryQuery {
        kinds: vec![record.kind.clone()],
        ..Default::default()
    };
    assert!(store.query(&by_kind, &cx).await.unwrap().is_empty());
}

async fn query_with_no_kinds_returns_every_kind(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let fact = record("fact", 1);
    let preference = record("preference", 2);
    store.put(fact.clone(), &cx).await.unwrap();
    store.put(preference.clone(), &cx).await.unwrap();

    let matches = store.query(&MemoryQuery::default(), &cx).await.unwrap();
    let mut ids: Vec<MemoryId> = matches.into_iter().map(|m| m.record.id).collect();
    ids.sort();
    let mut expected = vec![fact.id, preference.id];
    expected.sort();
    assert_eq!(ids, expected);
}

async fn query_filters_by_kind(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let fact = record("fact", 1);
    let preference = record("preference", 2);
    store.put(fact.clone(), &cx).await.unwrap();
    store.put(preference.clone(), &cx).await.unwrap();

    let query = MemoryQuery {
        kinds: vec![fact.kind.clone()],
        ..Default::default()
    };
    let matches = store.query(&query, &cx).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, fact.id);
}

async fn query_filters_by_metadata_exactly(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");

    let mut chat = record("fact", 1);
    chat.metadata.insert("source".into(), json!("chat"));
    let mut doc = record("fact", 2);
    doc.metadata.insert("source".into(), json!("document"));
    store.put(chat.clone(), &cx).await.unwrap();
    store.put(doc.clone(), &cx).await.unwrap();

    let mut metadata = serde_json::Map::new();
    metadata.insert("source".into(), json!("chat"));
    let query = MemoryQuery {
        metadata,
        ..Default::default()
    };
    let matches = store.query(&query, &cx).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, chat.id);
}

async fn query_respects_the_limit(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    for i in 1..=5u64 {
        store.put(record("fact", i), &cx).await.unwrap();
    }

    let query = MemoryQuery {
        limit: Some(2),
        ..Default::default()
    };
    let matches = store.query(&query, &cx).await.unwrap();
    assert_eq!(matches.len(), 2);
}

async fn query_ranks_most_recent_first(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let older = record("fact", 1);
    let newer = record("fact", 2);
    store.put(older.clone(), &cx).await.unwrap();
    store.put(newer.clone(), &cx).await.unwrap();

    let matches = store.query(&MemoryQuery::default(), &cx).await.unwrap();
    assert_eq!(matches[0].record.id, newer.id);
    assert_eq!(matches[1].record.id, older.id);
    assert!(matches.iter().all(|m| m.score.is_none()));
}

async fn semantic_query_fields_are_all_unsupported(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");

    for query in [
        MemoryQuery {
            text: Some("anything".into()),
            ..Default::default()
        },
        MemoryQuery {
            embedding: Some(vec![0.1, 0.2]),
            ..Default::default()
        },
        MemoryQuery {
            min_score: Some(0.5),
            ..Default::default()
        },
    ] {
        let error = store.query(&query, &cx).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    }
}

async fn an_expired_record_is_not_query_visible(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(2_000));
    let store = fixture.store();
    let cx = user("alice");
    let mut record = record("fact", 1);
    record.expires_at = Some(Timestamp::from_millis(1_500));
    store.put(record.clone(), &cx).await.unwrap();

    assert!(
        store
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn get_and_delete_ignore_expiry(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(2_000));
    let store = fixture.store();
    let cx = user("alice");
    let mut record = record("fact", 1);
    record.expires_at = Some(Timestamp::from_millis(1_500));
    store.put(record.clone(), &cx).await.unwrap();

    // Not query-visible, but still addressable by id: the sweep has not reached it yet, and
    // an exact lookup is not the visibility rule `query` enforces.
    assert_eq!(
        store.get(&record.id, &cx).await.unwrap(),
        Some(MemoryRecord {
            owner: PrincipalId::new("alice"),
            ..record.clone()
        })
    );
    assert!(store.delete(&record.id, &cx).await.unwrap());
    assert!(store.get(&record.id, &cx).await.unwrap().is_none());
}

async fn a_record_without_expiry_is_never_swept(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();
    let record = record("fact", 1);
    store.put(record.clone(), &user("alice")).await.unwrap();

    fixture.clock().advance(Duration::from_secs(3600));
    let removed = fixture.sweep().await;
    assert_eq!(removed, 0);
    assert!(
        store
            .get(&record.id, &user("alice"))
            .await
            .unwrap()
            .is_some()
    );
}

async fn sweeping_reclaims_an_expired_record(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();
    let cx = user("alice");

    let mut expired = record("fact", 1);
    expired.expires_at = Some(Timestamp::from_millis(1_500));
    let alive = record("fact", 2);
    store.put(expired.clone(), &cx).await.unwrap();
    store.put(alive.clone(), &cx).await.unwrap();

    fixture.clock().set(Timestamp::from_millis(2_000));
    let removed = fixture.sweep().await;
    assert_eq!(removed, 1);
    assert!(store.get(&expired.id, &cx).await.unwrap().is_none());
    assert!(store.get(&alive.id, &cx).await.unwrap().is_some());

    // A second sweep at the same time finds nothing left to do.
    assert_eq!(fixture.sweep().await, 0);
}

/// The case that loses data if a store keeps an expiry index and forgets to retire the old
/// entry: the record is still live, but something else in the database still claims it is due
/// at the earlier time, and the next sweep believes it.
async fn changing_an_expiry_retires_the_old_one(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();
    let cx = user("alice");

    let mut record = record("fact", 1);
    record.expires_at = Some(Timestamp::from_millis(1_500));
    store.put(record.clone(), &cx).await.unwrap();

    record.expires_at = Some(Timestamp::from_millis(3_000));
    store.put(record.clone(), &cx).await.unwrap();

    // Past the expiry the record was *first* given, and well short of the one it has now.
    fixture.clock().set(Timestamp::from_millis(2_000));
    assert_eq!(
        fixture.sweep().await,
        0,
        "the retracted expiry must not still be able to reclaim the record"
    );
    assert!(store.get(&record.id, &cx).await.unwrap().is_some());
    assert_eq!(
        store
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .len(),
        1,
        "the record is live until its current expiry, not its previous one"
    );

    // The replacement expiry is the one that counts, and it does count.
    fixture.clock().set(Timestamp::from_millis(3_000));
    assert_eq!(fixture.sweep().await, 1);
    assert!(store.get(&record.id, &cx).await.unwrap().is_none());
}

async fn clearing_an_expiry_makes_a_record_permanent(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();
    let cx = user("alice");

    let mut record = record("fact", 1);
    record.expires_at = Some(Timestamp::from_millis(1_500));
    store.put(record.clone(), &cx).await.unwrap();

    record.expires_at = None;
    store.put(record.clone(), &cx).await.unwrap();

    fixture.clock().advance(Duration::from_secs(3600));
    assert_eq!(
        fixture.sweep().await,
        0,
        "a record whose expiry was withdrawn must outlive it"
    );
    assert_eq!(
        store
            .get(&record.id, &cx)
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        None
    );
}

async fn adding_an_expiry_to_a_permanent_record_takes_effect(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();
    let cx = user("alice");

    let mut record = record("fact", 1);
    store.put(record.clone(), &cx).await.unwrap();

    record.expires_at = Some(Timestamp::from_millis(2_000));
    store.put(record.clone(), &cx).await.unwrap();

    fixture.clock().set(Timestamp::from_millis(2_000));
    assert_eq!(
        fixture.sweep().await,
        1,
        "an expiry added by a replacement must be reclaimable"
    );
    assert!(store.get(&record.id, &cx).await.unwrap().is_none());
}

async fn concurrent_puts_are_all_visible(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");

    let mut handles = Vec::new();
    for i in 0..32u64 {
        let store = store.clone();
        let cx = cx.clone();
        handles.push(tokio::spawn(async move {
            store.put(record("fact", i), &cx).await.unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let matches = store.query(&MemoryQuery::default(), &cx).await.unwrap();
    assert_eq!(
        matches.len(),
        32,
        "a concurrent put must not be lost or duplicated"
    );
}
