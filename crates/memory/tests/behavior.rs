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
use std::sync::Arc;

use support::{Backend, BrokenEmbedder, KeywordEmbedder, user};

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
    text_search_is_unsupported_without_an_embedding_model,
    min_score_without_anything_to_score_is_unsupported,
    a_precomputed_embedding_ranks_without_an_embedding_model,
    text_search_ranks_by_similarity,
    min_score_drops_weak_matches,
    an_unembedded_record_is_absent_from_a_semantic_result,
    exact_filters_still_apply_to_a_semantic_query,
    a_semantic_query_reports_a_score_and_an_exact_one_does_not,
    a_caller_supplied_embedding_is_kept_rather_than_recomputed,
    a_write_fails_when_the_embedding_model_is_unreachable,
    capabilities_report_whether_text_search_is_available,
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

async fn text_search_is_unsupported_without_an_embedding_model(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let query = MemoryQuery {
        text: Some("anything".into()),
        ..Default::default()
    };
    let error = store.query(&query, &user("alice")).await.unwrap_err();
    // Not an empty result and not the most recent records: a store that cannot search says
    // so, because either fallback would read as a memory that had forgotten.
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

async fn min_score_without_anything_to_score_is_unsupported(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let query = MemoryQuery {
        min_score: Some(0.5),
        ..Default::default()
    };
    let error = store.query(&query, &user("alice")).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

/// Ranking a vector is arithmetic over what is already stored, so it needs no model at all.
async fn a_precomputed_embedding_ranks_without_an_embedding_model(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");

    let mut near = record("fact", 1);
    near.embedding = Some(vec![1.0, 0.0]);
    let mut far = record("fact", 2);
    far.embedding = Some(vec![-1.0, 0.0]);
    store.put(near.clone(), &cx).await.unwrap();
    store.put(far.clone(), &cx).await.unwrap();

    let matches = store
        .query(
            &MemoryQuery {
                embedding: Some(vec![1.0, 0.0]),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();

    let ids: Vec<_> = matches.iter().map(|matched| matched.record.id).collect();
    assert_eq!(ids, vec![near.id, far.id]);
    assert_eq!(matches[0].score, Some(1.0));
    assert_eq!(matches[1].score, Some(-1.0));
}

async fn text_search_ranks_by_similarity(backend: Backend) {
    let embedder = Arc::new(KeywordEmbedder::new());
    let fixture = backend.embedding(embedder.clone());
    let store = fixture.store();
    let cx = user("alice");

    let tea = MemoryRecord::new("fact", json!("alice drinks tea"), Timestamp::from_millis(1));
    let coffee = MemoryRecord::new(
        "fact",
        json!("bob drinks coffee"),
        Timestamp::from_millis(2),
    );
    store.put(tea.clone(), &cx).await.unwrap();
    store.put(coffee.clone(), &cx).await.unwrap();

    let matches = store
        .query(
            &MemoryQuery {
                text: Some("tea".into()),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();

    let ids: Vec<_> = matches.iter().map(|matched| matched.record.id).collect();
    // The coffee record is *newer*, so this is also the assertion that similarity beat
    // recency rather than agreeing with it by luck.
    assert_eq!(ids, vec![tea.id, coffee.id]);
    assert!(
        matches[0].score.unwrap() > matches[1].score.unwrap(),
        "{:?}",
        matches.iter().map(|m| m.score).collect::<Vec<_>>()
    );
}

async fn min_score_drops_weak_matches(backend: Backend) {
    let fixture = backend.embedding(Arc::new(KeywordEmbedder::new()));
    let store = fixture.store();
    let cx = user("alice");

    let tea = MemoryRecord::new("fact", json!("alice drinks tea"), Timestamp::from_millis(1));
    let coffee = MemoryRecord::new(
        "fact",
        json!("bob drinks coffee"),
        Timestamp::from_millis(2),
    );
    store.put(tea.clone(), &cx).await.unwrap();
    store.put(coffee.clone(), &cx).await.unwrap();

    let matches = store
        .query(
            &MemoryQuery {
                text: Some("tea".into()),
                min_score: Some(0.99),
                limit: Some(10),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();

    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].record.id, tea.id);
}

/// A record nothing measured is not a weak match; it is not a match.
async fn an_unembedded_record_is_absent_from_a_semantic_result(backend: Backend) {
    let plain = backend.open();
    let cx = user("alice");
    let orphan = MemoryRecord::new("fact", json!("alice drinks tea"), Timestamp::from_millis(1));
    plain.store().put(orphan.clone(), &cx).await.unwrap();

    // The store the record was written through had no model, so nothing embedded it. Query
    // it as a vector, which needs no model either.
    let matches = plain
        .store()
        .query(
            &MemoryQuery {
                embedding: Some(KeywordEmbedder::vector("tea")),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();
    assert!(matches.is_empty(), "{matches:?}");

    // And it is still there, and still findable the exact way.
    assert!(plain.store().get(&orphan.id, &cx).await.unwrap().is_some());
    assert_eq!(
        plain
            .store()
            .query(&MemoryQuery::default(), &cx)
            .await
            .unwrap()
            .len(),
        1
    );
}

async fn exact_filters_still_apply_to_a_semantic_query(backend: Backend) {
    let fixture = backend.embedding(Arc::new(KeywordEmbedder::new()));
    let store = fixture.store();
    let cx = user("alice");

    let fact = MemoryRecord::new("fact", json!("alice drinks tea"), Timestamp::from_millis(1));
    let mut preference = MemoryRecord::new(
        "preference",
        json!("alice drinks tea"),
        Timestamp::from_millis(2),
    );
    preference
        .metadata
        .insert("source".to_owned(), json!("chat"));
    store.put(fact.clone(), &cx).await.unwrap();
    store.put(preference.clone(), &cx).await.unwrap();

    let matches = store
        .query(
            &MemoryQuery {
                kinds: vec![preference.kind.clone()],
                text: Some("tea".into()),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, preference.id);

    let mut metadata = serde_json::Map::new();
    metadata.insert("source".to_owned(), json!("chat"));
    let matches = store
        .query(
            &MemoryQuery {
                metadata,
                text: Some("tea".into()),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].record.id, preference.id);
}

async fn a_semantic_query_reports_a_score_and_an_exact_one_does_not(backend: Backend) {
    let fixture = backend.embedding(Arc::new(KeywordEmbedder::new()));
    let store = fixture.store();
    let cx = user("alice");
    store
        .put(
            MemoryRecord::new("fact", json!("alice drinks tea"), Timestamp::from_millis(1)),
            &cx,
        )
        .await
        .unwrap();

    let exact = store.query(&MemoryQuery::default(), &cx).await.unwrap();
    assert_eq!(exact[0].score, None);

    let semantic = store
        .query(
            &MemoryQuery {
                text: Some("tea".into()),
                ..Default::default()
            },
            &cx,
        )
        .await
        .unwrap();
    assert!(semantic[0].score.is_some());
}

async fn a_caller_supplied_embedding_is_kept_rather_than_recomputed(backend: Backend) {
    let embedder = Arc::new(KeywordEmbedder::new());
    let fixture = backend.embedding(embedder.clone());
    let store = fixture.store();
    let cx = user("alice");

    let mut record = record("fact", 1);
    record.embedding = Some(vec![0.25, 0.75]);
    store.put(record.clone(), &cx).await.unwrap();

    assert_eq!(embedder.calls(), 0, "nothing needed embedding");
    assert_eq!(
        store.get(&record.id, &cx).await.unwrap().unwrap().embedding,
        Some(vec![0.25, 0.75])
    );
}

/// A record stored without a vector would be invisible to every future search, so the write
/// fails instead — visibly, now, rather than silently for the life of the record.
async fn a_write_fails_when_the_embedding_model_is_unreachable(backend: Backend) {
    let fixture = backend.embedding(Arc::new(BrokenEmbedder));
    let store = fixture.store();
    let cx = user("alice");

    let record = record("fact", 1);
    assert!(store.put(record.clone(), &cx).await.is_err());
    assert!(
        store.get(&record.id, &cx).await.unwrap().is_none(),
        "a refused write must leave nothing behind"
    );
}

async fn capabilities_report_whether_text_search_is_available(backend: Backend) {
    assert!(!backend.open().store().capabilities().semantic_text);
    assert!(
        backend
            .embedding(Arc::new(KeywordEmbedder::new()))
            .store()
            .capabilities()
            .semantic_text
    );
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
