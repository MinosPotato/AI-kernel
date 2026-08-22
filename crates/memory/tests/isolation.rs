//! Who may reach a memory record, and who may not.
//!
//! A [`MemoryStore`] holds records for more than one principal in one file. Everything here
//! is about the line between them: that the store decides who owns a record rather than
//! believing the record, that naming another principal's record is refused rather than
//! silently served, and that enumerating cannot be used to find out what other principals
//! have remembered.
//!
//! This is the memory counterpart of `aik-context`'s `isolation.rs`, and it asks the same
//! questions in the same order on purpose: both subsystems answer them through the single
//! [`Principal::may_act_for`](aik_api::permission::Principal::may_act_for) rule, so an
//! assertion that holds for one and not the other means that rule has been applied twice
//! rather than shared.

use aik_api::memory::{MemoryQuery, MemoryRecord};
use aik_api::permission::PrincipalId;
use aik_core::ErrorKind;
use aik_core::clock::Timestamp;
use serde_json::json;

mod support;
use support::{Backend, agent_for, anonymous, user};

crate::both_backends!(
    the_owner_comes_from_the_context_not_the_record,
    another_principal_cannot_read_a_record,
    another_principal_cannot_delete_a_record,
    another_principal_cannot_replace_a_record,
    a_query_returns_only_the_callers_own_records,
    an_agent_acting_on_behalf_of_the_owner_may_use_the_record,
    delegation_does_not_run_the_other_way,
    replacing_a_record_does_not_transfer_it,
    the_system_principal_is_an_identity_not_a_wildcard,
    a_deleted_record_s_id_can_be_reused_by_someone_else,
);

fn record(kind: &str, created_at_ms: u64) -> MemoryRecord {
    MemoryRecord::new(
        kind,
        json!({ "n": created_at_ms }),
        Timestamp::from_millis(created_at_ms),
    )
}

async fn the_owner_comes_from_the_context_not_the_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    // A caller that names someone else as the owner is not believed. This is the memory
    // analogue of a context record's attribution: whatever produced the content does not get
    // to choose whose it becomes.
    let mut forged = record("fact", 1);
    forged.owner = PrincipalId::new("mallory");
    store.put(forged.clone(), &user("alice")).await.unwrap();

    let stored = store
        .get(&forged.id, &user("alice"))
        .await
        .unwrap()
        .expect("alice stored it, so alice can read it");
    assert_eq!(stored.owner, PrincipalId::new("alice"));

    let error = store.get(&forged.id, &user("mallory")).await.unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Permission,
        "naming yourself as owner must not make you one"
    );
}

async fn another_principal_cannot_read_a_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let stored = record("fact", 1);
    store.put(stored.clone(), &user("alice")).await.unwrap();

    let error = store.get(&stored.id, &user("mallory")).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(
        error.to_string().contains("alice"),
        "the refusal should name the owner it is protecting: {error}"
    );
}

async fn another_principal_cannot_delete_a_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let stored = record("fact", 1);
    store.put(stored.clone(), &user("alice")).await.unwrap();

    let error = store
        .delete(&stored.id, &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);

    // Refusing has to actually leave it there. A delete that removed the row and then
    // reported a permission error would be the worst of both.
    assert!(
        store
            .get(&stored.id, &user("alice"))
            .await
            .unwrap()
            .is_some()
    );
}

async fn another_principal_cannot_replace_a_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let mut stored = record("fact", 1);
    store.put(stored.clone(), &user("alice")).await.unwrap();

    stored.content = json!({ "n": "replaced" });
    let error = store
        .put(stored.clone(), &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Permission,
        "an id collision must not be a way to overwrite someone else's memory"
    );

    let kept = store
        .get(&stored.id, &user("alice"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.content, json!({ "n": 1 }), "the original survived");
    assert_eq!(kept.owner, PrincipalId::new("alice"));
}

async fn a_query_returns_only_the_callers_own_records(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let hers = record("fact", 1);
    let his = record("fact", 2);
    store.put(hers.clone(), &user("alice")).await.unwrap();
    store.put(his.clone(), &user("bob")).await.unwrap();

    // Absent rather than refused: a query that errored on encountering someone else's record
    // would report that the record exists, which is the thing being withheld.
    let found = store
        .query(&MemoryQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].record.id, hers.id);

    // Same through the kind index, which is the other path candidates can arrive by.
    let by_kind = MemoryQuery {
        kinds: vec![hers.kind.clone()],
        ..Default::default()
    };
    let found = store.query(&by_kind, &user("bob")).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].record.id, his.id);
}

async fn an_agent_acting_on_behalf_of_the_owner_may_use_the_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let stored = record("fact", 1);
    store.put(stored.clone(), &user("alice")).await.unwrap();

    let agent = agent_for("assistant", "alice");
    assert!(store.get(&stored.id, &agent).await.unwrap().is_some());

    let found = store.query(&MemoryQuery::default(), &agent).await.unwrap();
    assert_eq!(
        found.len(),
        1,
        "an agent working for Alice sees what Alice remembered"
    );
    assert!(store.delete(&stored.id, &agent).await.unwrap());
}

async fn delegation_does_not_run_the_other_way(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    // The agent stores something of its own while working for Alice. It is the agent's, not
    // Alice's: acting for someone does not put your own memories in their hands.
    let stored = record("fact", 1);
    store
        .put(stored.clone(), &agent_for("assistant", "alice"))
        .await
        .unwrap();
    assert_eq!(
        store
            .get(&stored.id, &agent_for("assistant", "alice"))
            .await
            .unwrap()
            .unwrap()
            .owner,
        PrincipalId::new("assistant")
    );

    let error = store.get(&stored.id, &user("alice")).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(
        store
            .query(&MemoryQuery::default(), &user("alice"))
            .await
            .unwrap()
            .is_empty()
    );
}

async fn replacing_a_record_does_not_transfer_it(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let mut stored = record("fact", 1);
    store.put(stored.clone(), &user("alice")).await.unwrap();

    // Revised by an agent working for Alice, which is allowed. If the store re-stamped the
    // owner from the caller, the very delegation that let the agent help would quietly move
    // the memory out of Alice's hands and into the agent's.
    stored.content = json!({ "n": "revised" });
    store
        .put(stored.clone(), &agent_for("assistant", "alice"))
        .await
        .unwrap();

    let kept = store
        .get(&stored.id, &user("alice"))
        .await
        .unwrap()
        .expect("it is still Alice's");
    assert_eq!(kept.owner, PrincipalId::new("alice"));
    assert_eq!(kept.content, json!({ "n": "revised" }));

    // And it did not end up counted under the agent instead, which a stale owner index would
    // show up as here and nowhere else.
    let hers = store
        .query(&MemoryQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(hers.len(), 1);
    assert_eq!(hers[0].record.id, stored.id);
}

async fn the_system_principal_is_an_identity_not_a_wildcard(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let hers = record("fact", 1);
    store.put(hers.clone(), &user("alice")).await.unwrap();

    // A context with no principal is the system acting for itself. That is an identity, so
    // it gets its own records and none of anyone else's.
    let its_own = record("fact", 2);
    store.put(its_own.clone(), &anonymous()).await.unwrap();
    assert_eq!(
        store
            .get(&its_own.id, &anonymous())
            .await
            .unwrap()
            .unwrap()
            .owner,
        PrincipalId::new("system")
    );

    let error = store.get(&hers.id, &anonymous()).await.unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Permission,
        "the system principal must not be able to read every user's memories"
    );

    let found = store
        .query(&MemoryQuery::default(), &anonymous())
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].record.id, its_own.id);
}

async fn a_deleted_record_s_id_can_be_reused_by_someone_else(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let stored = record("fact", 1);

    store.put(stored.clone(), &user("alice")).await.unwrap();
    assert!(store.delete(&stored.id, &user("alice")).await.unwrap());

    // Ownership is a property of the record, not a reservation on the id. Once the record is
    // gone the id is free, exactly as a cleared context session can be reclaimed.
    store.put(stored.clone(), &user("bob")).await.unwrap();
    assert_eq!(
        store
            .get(&stored.id, &user("bob"))
            .await
            .unwrap()
            .unwrap()
            .owner,
        PrincipalId::new("bob")
    );
}
