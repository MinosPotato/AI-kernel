//! Who may see what, and what nobody may hide.
//!
//! The audit trail is the record of this system's authority. Two properties make it worth
//! keeping, and both are security properties rather than features:
//!
//! * a reader sees their own trail and no one else's, however they phrase the question;
//! * nobody — no filter, no principal, no retention sweep — can make the trail *look*
//!   complete when it is not.
//!
//! Everything here runs against both backends, because a rule enforced in one store and not
//! the other is not a rule.

mod support;

use aik_api::audit::{
    AuditEntry, AuditEntryKind, AuditGap, AuditQuery, AuthorizationOutcome, InvocationOutcome,
};
use aik_api::permission::PrincipalId;
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use support::{Backend, agent_for, allowed, anonymous, decision, invocation, user};

both_backends!(
    a_reader_sees_only_what_they_are_a_party_to,
    a_principal_filter_cannot_be_used_to_read_another_principals_trail,
    a_correlation_filter_cannot_be_used_to_read_another_principals_trail,
    the_delegating_principal_sees_what_was_done_on_their_behalf,
    delegated_authority_reaches_the_trail_it_was_delegated_by,
    a_delegators_view_stops_at_what_was_done_for_them,
    the_system_principal_is_not_a_master_key,
    a_gap_is_visible_to_every_reader_however_they_filtered,
    a_retention_marker_survives_the_sweep_that_wrote_it,
    a_sweep_never_removes_a_gap,
    a_sweep_bounded_by_a_cutoff_leaves_everything_after_it,
    a_swept_trail_never_reuses_a_sequence_number,
    an_invisible_record_is_absent_rather_than_an_error,
    a_preview_counts_what_a_sweep_would_remove_without_removing_it,
    a_principal_filter_finds_what_was_done_on_that_principals_behalf,
);

async fn a_reader_sees_only_what_they_are_a_party_to(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();
    store
        .append(allowed("mallory", "fs.read", 20))
        .await
        .unwrap();

    let seen = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].entry.principal(), PrincipalId::new("alice"));
}

async fn a_principal_filter_cannot_be_used_to_read_another_principals_trail(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();

    // Naming someone else in a filter narrows what the reader may see; it can never widen it.
    let seen = store
        .query(
            &AuditQuery {
                principal: Some(PrincipalId::new("alice")),
                ..AuditQuery::default()
            },
            &user("mallory"),
        )
        .await
        .unwrap();
    assert!(
        seen.is_empty(),
        "a filter is not a permission: mallory must not read alice's trail by asking for it"
    );
}

async fn a_correlation_filter_cannot_be_used_to_read_another_principals_trail(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let correlation = CorrelationId::new();

    store
        .append(decision(
            "alice",
            None,
            "fs.read",
            10,
            correlation,
            AuthorizationOutcome::Allowed,
        ))
        .await
        .unwrap();

    // A correlation id is not a secret — it appears in logs and in error messages — so
    // knowing one must not be a way around the visibility rule.
    let seen = store
        .query(
            &AuditQuery {
                correlation: Some(correlation),
                ..AuditQuery::default()
            },
            &user("mallory"),
        )
        .await
        .unwrap();
    assert!(seen.is_empty());
}

async fn the_delegating_principal_sees_what_was_done_on_their_behalf(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store
        .append(invocation(
            "assistant",
            Some("alice"),
            "fs.write",
            10,
            CorrelationId::new(),
            InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();

    // The whole point of recording delegation: the human an agent acts for can review it.
    let seen = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(seen.len(), 1);

    // And the agent sees its own.
    let by_agent = store
        .query(&AuditQuery::default(), &user("assistant"))
        .await
        .unwrap();
    assert_eq!(by_agent.len(), 1);
}

/// An agent carrying alice's delegated authority reads alice's trail, exactly as it reads
/// every other resource alice owns.
///
/// This is [`Principal::may_act_for`](aik_api::permission::Principal::may_act_for) — the one
/// rule the context store, the memory store and this one all ask — and the audit trail
/// deliberately does not get a second, narrower version of it. Two implementations of a
/// security rule are two things to keep in step, with a divergence nobody notices until it
/// lets one principal read another's data.
///
/// What keeps that from being a way for a *model* to read the trail is not this rule but the
/// absence of a door: no tool exposes the audit store, so the only reader is an operator at
/// the CLI. `no_audit_tool_is_registered` in `aik-cli`'s security suite is where that is
/// pinned down.
async fn delegated_authority_reaches_the_trail_it_was_delegated_by(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();

    let seen = store
        .query(&AuditQuery::default(), &agent_for("assistant", "alice"))
        .await
        .unwrap();
    assert_eq!(seen.len(), 1);
}

/// Delegation still runs one way: alice sees what was done *for* her, never what the agent
/// did on its own account.
async fn a_delegators_view_stops_at_what_was_done_for_them(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    // The agent acting autonomously — no `on_behalf_of` — which is a different principal
    // doing a different thing.
    store
        .append(allowed("assistant", "fs.read", 10))
        .await
        .unwrap();

    let seen = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert!(
        seen.is_empty(),
        "being acted for is not the same as being the actor"
    );
}

async fn the_system_principal_is_not_a_master_key(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();

    // A context naming no principal is the system acting for itself — an identity like any
    // other, not a wildcard, and certainly not an auditor's badge.
    let seen = store
        .query(&AuditQuery::default(), &anonymous())
        .await
        .unwrap();
    assert!(seen.is_empty());
}

async fn a_gap_is_visible_to_every_reader_however_they_filtered(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();
    store
        .append(AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(20),
            missed: 12,
        }))
        .await
        .unwrap();

    for (label, reader) in [
        ("a stranger", user("mallory")),
        ("the subject", user("alice")),
        ("the system", anonymous()),
    ] {
        let seen = store.query(&AuditQuery::default(), &reader).await.unwrap();
        assert!(
            seen.iter()
                .any(|record| record.entry.kind() == AuditEntryKind::Gap),
            "{label} must be able to tell that the trail is incomplete"
        );
    }

    // Including through a principal filter that names somebody the gap has nothing to do
    // with: a filtered view that hid the gap would read as a complete account of a period
    // it cannot account for.
    let filtered = store
        .query(
            &AuditQuery {
                principal: Some(PrincipalId::new("alice")),
                ..AuditQuery::default()
            },
            &user("alice"),
        )
        .await
        .unwrap();
    assert!(
        filtered
            .iter()
            .any(|record| record.entry.kind() == AuditEntryKind::Gap)
    );
}

async fn a_retention_marker_survives_the_sweep_that_wrote_it(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();
    assert_eq!(fixture.sweep(Timestamp::from_millis(100)).await, 1);

    let seen = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert_eq!(seen.len(), 1, "the record went, the account of it did not");
    let AuditEntry::Retention(applied) = &seen[0].entry else {
        panic!("expected a retention marker, got {:?}", seen[0].entry);
    };
    assert_eq!(applied.removed, 1);
    assert_eq!(applied.cutoff, Timestamp::from_millis(100));
    assert_eq!(
        applied.timestamp,
        Timestamp::from_millis(1_000),
        "the marker is stamped with when the sweep ran, not with the cutoff"
    );

    // And a second sweep past the marker's own timestamp does not remove it either.
    assert_eq!(fixture.sweep(Timestamp::from_millis(100_000)).await, 0);
    assert_eq!(
        store
            .query(&AuditQuery::default(), &user("alice"))
            .await
            .unwrap()
            .len(),
        1
    );
}

async fn a_sweep_never_removes_a_gap(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(1_000));
    let store = fixture.store();

    store
        .append(AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(1),
            missed: 3,
        }))
        .await
        .unwrap();
    store.append(allowed("alice", "fs.read", 2)).await.unwrap();

    assert_eq!(
        fixture.sweep(Timestamp::from_millis(500)).await,
        1,
        "only the ordinary record is sweepable"
    );

    let seen = store
        .query(&AuditQuery::default(), &user("alice"))
        .await
        .unwrap();
    assert!(
        seen.iter()
            .any(|record| record.entry.kind() == AuditEntryKind::Gap),
        "retention must not erase the evidence that the trail was already incomplete"
    );
}

async fn a_sweep_bounded_by_a_cutoff_leaves_everything_after_it(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(10_000));
    let store = fixture.store();

    for at in [10, 20, 30, 40, 50] {
        store.append(allowed("alice", "fs.read", at)).await.unwrap();
    }

    assert_eq!(fixture.sweep(Timestamp::from_millis(30)).await, 3);

    let seen = store
        .query(
            &AuditQuery {
                kinds: vec![AuditEntryKind::Authorization],
                ..AuditQuery::default()
            },
            &user("alice"),
        )
        .await
        .unwrap();
    let timestamps: Vec<u64> = seen
        .iter()
        .map(|record| record.entry.timestamp().as_millis())
        .collect();
    assert_eq!(timestamps, vec![50, 40]);
}

async fn a_swept_trail_never_reuses_a_sequence_number(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(10_000));
    let store = fixture.store();

    for at in [10, 20, 30] {
        store.append(allowed("alice", "fs.read", at)).await.unwrap();
    }
    assert_eq!(fixture.sweep(Timestamp::from_millis(30)).await, 3);

    // The marker took 4; the next real record must take 5, not 1. A reused number would make
    // two different records indistinguishable in an exported trail.
    let next = store.append(allowed("alice", "fs.read", 40)).await.unwrap();
    assert_eq!(next, 5);
    assert_eq!(store.last_sequence().await.unwrap(), 5);
}

async fn an_invisible_record_is_absent_rather_than_an_error(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("alice", "fs.read", 10)).await.unwrap();

    // Erroring on somebody else's record would confirm that it exists, which is exactly the
    // fact a stranger must not be able to establish.
    let result = store.query(&AuditQuery::default(), &user("mallory")).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(result.unwrap().is_empty());
}

async fn a_preview_counts_what_a_sweep_would_remove_without_removing_it(backend: Backend) {
    let fixture = backend.at(Timestamp::from_millis(10_000));
    let store = fixture.store();

    for at in [10, 20, 30, 40] {
        store.append(allowed("alice", "fs.read", at)).await.unwrap();
    }
    store
        .append(AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(15),
            missed: 2,
        }))
        .await
        .unwrap();

    let sweeper = fixture.sweeper();
    let cutoff = Timestamp::from_millis(30);
    assert_eq!(
        sweeper.count_older_than(cutoff).await.unwrap(),
        3,
        "the gap at 15ms is not sweepable and must not be counted as though it were"
    );

    // And counting really did not remove anything.
    assert_eq!(store.last_sequence().await.unwrap(), 5);
    assert_eq!(fixture.sweep(cutoff).await, 3, "the preview was accurate");
}

/// A principal filter reaches delegated records, not only records the principal is the actor
/// of.
///
/// Worth its own test because the durable store answers this from an index rather than from a
/// scan: a record naming an actor and a delegator has to be indexed under both, and an index
/// that recorded only the actor would make this query come back empty while the unfiltered one
/// still worked — a hole nothing else here would notice.
async fn a_principal_filter_finds_what_was_done_on_that_principals_behalf(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store
        .append(invocation(
            "assistant",
            Some("alice"),
            "fs.write",
            10,
            CorrelationId::new(),
            InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();

    let found = store
        .query(
            &AuditQuery {
                principal: Some(PrincipalId::new("alice")),
                ..AuditQuery::default()
            },
            &user("alice"),
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);

    // And the same filter under the actor's own name finds it too.
    let by_actor = store
        .query(
            &AuditQuery {
                principal: Some(PrincipalId::new("assistant")),
                ..AuditQuery::default()
            },
            &user("alice"),
        )
        .await
        .unwrap();
    assert_eq!(by_actor.len(), 1);
}
