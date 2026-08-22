//! What an [`AuditStore`] does, whichever implementation it is.
//!
//! Every test here runs twice — once against the in-memory store, once against the durable one
//! — so that the two cannot drift. Assertions that only mean something on disk live in
//! `persistence.rs`; assertions about who may see what live in `security.rs`.

mod support;

use aik_api::audit::{AuditEntry, AuditEntryKind, AuditGap, AuditQuery, InvocationOutcome};
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use support::{
    Backend, agent_for, allowed, anonymous, decision, denied, invocation, invoked, user,
};

both_backends!(
    a_fresh_trail_is_empty,
    sequence_numbers_start_at_one_and_never_repeat,
    records_come_back_newest_first,
    a_limit_takes_the_newest_records_not_the_oldest,
    every_filter_narrows_the_answer,
    a_correlation_joins_decisions_to_the_invocation_they_gated,
    a_time_range_is_inclusive_at_both_ends,
    refusals_can_be_read_on_their_own,
    an_entry_comes_back_exactly_as_it_went_in,
    the_last_sequence_tracks_the_trail,
);

async fn a_fresh_trail_is_empty(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    assert!(
        store
            .query(&AuditQuery::default(), &anonymous())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.last_sequence().await.unwrap(), 0);
}

async fn sequence_numbers_start_at_one_and_never_repeat(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let mut seen = Vec::new();
    for at in 0..5 {
        seen.push(store.append(allowed("agent", "fs.read", at)).await.unwrap());
    }

    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}

async fn records_come_back_newest_first(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    for at in 0..4 {
        store.append(allowed("agent", "fs.read", at)).await.unwrap();
    }

    let found = store
        .query(&AuditQuery::default(), &user("agent"))
        .await
        .unwrap();
    let sequences: Vec<u64> = found.iter().map(|record| record.sequence).collect();
    assert_eq!(sequences, vec![4, 3, 2, 1]);
}

async fn a_limit_takes_the_newest_records_not_the_oldest(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    for at in 0..10 {
        store.append(allowed("agent", "fs.read", at)).await.unwrap();
    }

    let found = store
        .query(
            &AuditQuery {
                limit: Some(3),
                ..AuditQuery::default()
            },
            &user("agent"),
        )
        .await
        .unwrap();

    let sequences: Vec<u64> = found.iter().map(|record| record.sequence).collect();
    assert_eq!(
        sequences,
        vec![10, 9, 8],
        "a truncated review must show the most recent activity, not the oldest"
    );
}

async fn every_filter_narrows_the_answer(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("agent", "fs.read", 10)).await.unwrap();
    store.append(denied("agent", "fs.write", 20)).await.unwrap();
    store.append(invoked("agent", "fs.read", 30)).await.unwrap();

    let reader = user("agent");
    let all = store.query(&AuditQuery::default(), &reader).await.unwrap();
    assert_eq!(all.len(), 3);

    let by_tool = store
        .query(
            &AuditQuery {
                tool: Some(aik_api::tool::ToolName::new("fs.write")),
                ..AuditQuery::default()
            },
            &reader,
        )
        .await
        .unwrap();
    assert_eq!(by_tool.len(), 1);
    assert_eq!(by_tool[0].sequence, 2);

    let by_kind = store
        .query(
            &AuditQuery {
                kinds: vec![AuditEntryKind::Invocation],
                ..AuditQuery::default()
            },
            &reader,
        )
        .await
        .unwrap();
    assert_eq!(by_kind.len(), 1);
    assert_eq!(by_kind[0].entry.kind(), AuditEntryKind::Invocation);

    let nobody = store
        .query(
            &AuditQuery {
                principal: Some(aik_api::permission::PrincipalId::new("someone-else")),
                ..AuditQuery::default()
            },
            &reader,
        )
        .await
        .unwrap();
    assert!(nobody.is_empty());
}

async fn a_correlation_joins_decisions_to_the_invocation_they_gated(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let correlation = CorrelationId::new();
    let unrelated = CorrelationId::new();

    store
        .append(decision(
            "agent",
            None,
            "fs.read",
            10,
            correlation,
            aik_api::audit::AuthorizationOutcome::Allowed,
        ))
        .await
        .unwrap();
    store
        .append(invocation(
            "agent",
            None,
            "fs.read",
            11,
            correlation,
            InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();
    store
        .append(invocation(
            "agent",
            None,
            "fs.list",
            12,
            unrelated,
            InvocationOutcome::Succeeded,
        ))
        .await
        .unwrap();

    let found = store
        .query(
            &AuditQuery {
                correlation: Some(correlation),
                ..AuditQuery::default()
            },
            &user("agent"),
        )
        .await
        .unwrap();

    assert_eq!(found.len(), 2, "one decision and the call it gated");
    assert!(
        found
            .iter()
            .all(|record| record.entry.correlation() == Some(correlation))
    );
}

async fn a_time_range_is_inclusive_at_both_ends(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    for at in [10, 20, 30, 40] {
        store.append(allowed("agent", "fs.read", at)).await.unwrap();
    }

    let found = store
        .query(
            &AuditQuery {
                since: Some(Timestamp::from_millis(20)),
                until: Some(Timestamp::from_millis(30)),
                ..AuditQuery::default()
            },
            &user("agent"),
        )
        .await
        .unwrap();

    let timestamps: Vec<u64> = found
        .iter()
        .map(|record| record.entry.timestamp().as_millis())
        .collect();
    assert_eq!(timestamps, vec![30, 20]);
}

async fn refusals_can_be_read_on_their_own(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    store.append(allowed("agent", "fs.read", 10)).await.unwrap();
    store.append(denied("agent", "fs.write", 20)).await.unwrap();
    store
        .append(invocation(
            "agent",
            None,
            "fs.write",
            21,
            CorrelationId::new(),
            InvocationOutcome::Denied,
        ))
        .await
        .unwrap();
    // A tool that broke is not a refusal: an operator asking what was *not allowed* must not
    // have to read past broken plumbing to find it.
    store
        .append(invocation(
            "agent",
            None,
            "fs.read",
            22,
            CorrelationId::new(),
            InvocationOutcome::Failed {
                kind: "timeout".into(),
            },
        ))
        .await
        .unwrap();

    let found = store
        .query(
            &AuditQuery {
                refusals_only: true,
                ..AuditQuery::default()
            },
            &user("agent"),
        )
        .await
        .unwrap();

    let sequences: Vec<u64> = found.iter().map(|record| record.sequence).collect();
    assert_eq!(sequences, vec![3, 2]);
}

async fn an_entry_comes_back_exactly_as_it_went_in(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    let entries = vec![
        decision(
            "agent",
            Some("alice"),
            "fs.write",
            10,
            CorrelationId::new(),
            aik_api::audit::AuthorizationOutcome::Denied {
                reason: "outside the workspace".into(),
            },
        ),
        invocation(
            "agent",
            Some("alice"),
            "fs.write",
            11,
            CorrelationId::new(),
            InvocationOutcome::Failed {
                kind: "confinement".into(),
            },
        ),
        AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(12),
            missed: 4,
        }),
    ];

    for entry in &entries {
        store.append(entry.clone()).await.unwrap();
    }

    let found = store
        .query(&AuditQuery::default(), &agent_for("agent", "alice"))
        .await
        .unwrap();
    let recovered: Vec<AuditEntry> = found.into_iter().rev().map(|record| record.entry).collect();
    assert_eq!(recovered, entries);
}

async fn the_last_sequence_tracks_the_trail(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();

    assert_eq!(store.last_sequence().await.unwrap(), 0);
    store.append(allowed("agent", "fs.read", 1)).await.unwrap();
    assert_eq!(store.last_sequence().await.unwrap(), 1);
    store.append(invoked("agent", "fs.read", 2)).await.unwrap();
    assert_eq!(store.last_sequence().await.unwrap(), 2);
}
