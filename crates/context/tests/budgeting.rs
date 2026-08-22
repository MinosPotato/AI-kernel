//! What a budget actually buys, over a conversation long enough for it to matter.
//!
//! These run against both stores. For the in-memory one they are what they look like;
//! for the durable one they double as a round-trip test, since a record whose pinning,
//! ordering or token cost did not survive being written and read back would fail them.

use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextEntry, ContextWindow, ELISION_MARKER};
use aik_api::model::{ContentPart, Message, Role};
use aik_api::permission::PrincipalId;
use aik_core::ErrorKind;
use serde_json::json;

mod support;
use support::{Backend, say, tool_exchange, user};

crate::both_backends!(
    a_long_conversation_costs_a_bounded_amount_per_turn,
    the_system_prompt_survives_every_turn,
    an_elided_tool_result_stays_retrievable_in_full,
    a_window_is_derived_not_stored,
    a_full_session_refuses_further_appends,
    a_refused_first_append_does_not_create_the_session,
    clearing_removes_the_session,
    a_cleared_session_can_be_reclaimed_by_a_different_principal,
    an_unknown_session_has_no_stats_and_an_empty_window,
    stats_report_full_fidelity_totals,
);

async fn a_long_conversation_costs_a_bounded_amount_per_turn(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    store
        .append(
            &session,
            ContextEntry::new(Message::text(Role::System, "You are a careful assistant.")).pinned(),
            &cx,
        )
        .await
        .unwrap();

    // 4 bytes per token, so this bound fits a handful of turns, not forty.
    let budget = ContextBudget::tokens(200).with_max_part_tokens(32);

    let mut peak = 0;
    for turn in 0..40 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("question {turn}"))),
                &cx,
            )
            .await
            .unwrap();

        let (call, result) = tool_exchange(&format!("call-{turn}"), &"x".repeat(4_000));
        store.append(&session, call, &cx).await.unwrap();
        store.append(&session, result, &cx).await.unwrap();
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::Assistant, format!("answer {turn}"))),
                &cx,
            )
            .await
            .unwrap();

        let window = store.window(&session, &budget, &cx).await.unwrap();
        assert!(
            window.usage.included_tokens <= 200,
            "turn {turn} exceeded the budget with {} tokens",
            window.usage.included_tokens
        );
        peak = peak.max(window.usage.included_tokens);
    }

    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert!(
        stats.tokens > 40_000,
        "the transcript should be genuinely large, was {}",
        stats.tokens
    );
    assert!(peak > 0, "the window should not have collapsed to nothing");

    // The whole point: what the model is sent stopped growing long before the transcript
    // did, and the transcript itself lost nothing.
    let window = store.window(&session, &budget, &cx).await.unwrap();
    assert!(window.usage.included_tokens * 50 < stats.tokens);
    assert_eq!(window.usage.total_tokens(), stats.tokens);
}

async fn the_system_prompt_survives_every_turn(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");
    let prompt = Message::text(Role::System, "You are a careful assistant.");

    store
        .append(&session, ContextEntry::new(prompt.clone()).pinned(), &cx)
        .await
        .unwrap();

    for turn in 0..30 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("question {turn}"))),
                &cx,
            )
            .await
            .unwrap();
    }

    let window = store
        .window(&session, &ContextBudget::tokens(40), &cx)
        .await
        .unwrap();
    assert_eq!(window.messages.first(), Some(&prompt));
    assert!(
        window.usage.dropped_records > 0,
        "turns should have been evicted"
    );
}

async fn an_elided_tool_result_stays_retrievable_in_full(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");
    let payload = "y".repeat(4_000);

    let (call, result) = tool_exchange("call-1", &payload);
    store.append(&session, call, &cx).await.unwrap();
    let stored = store.append(&session, result, &cx).await.unwrap();

    let budget = ContextBudget::default().with_max_part_tokens(16);
    let window = store.window(&session, &budget, &cx).await.unwrap();

    // The model sees a marker naming the record...
    let ContentPart::ToolResult { content, .. } = &window.messages[1].content[0] else {
        panic!("expected a tool result");
    };
    let named = content[ELISION_MARKER]["record"].as_str().unwrap();
    assert_eq!(named, stored.id.to_string());
    assert!(window.usage.included_tokens < 100);

    // ...and the kernel still has every byte of it.
    let full = store.get(&session, &stored.id, &cx).await.unwrap().unwrap();
    let ContentPart::ToolResult { content, .. } = &full.message.content[0] else {
        panic!("expected a tool result");
    };
    assert_eq!(content["content"], json!(payload));
}

async fn a_window_is_derived_not_stored(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    for turn in 0..10 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("question {turn}"))),
                &cx,
            )
            .await
            .unwrap();
    }

    // Two different budgets over the same records, in either order, and the records are
    // unchanged by either: assembling is a pure function of what is stored.
    let tight = store
        .window(&session, &ContextBudget::tokens(20), &cx)
        .await
        .unwrap();
    let loose = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    let tight_again = store
        .window(&session, &ContextBudget::tokens(20), &cx)
        .await
        .unwrap();

    assert_eq!(tight, tight_again);
    assert_eq!(loose.usage.included_records, 10);
    assert!(tight.usage.included_records < loose.usage.included_records);

    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.records, 10);
}

async fn a_full_session_refuses_further_appends(backend: Backend) {
    let fixture = backend.bounded(2);
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    store.append(&session, say("one"), &cx).await.unwrap();
    store.append(&session, say("two"), &cx).await.unwrap();

    let error = store.append(&session, say("three"), &cx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(error.to_string().contains("full"), "{error}");

    // A bound that half-applied an append would be worse than no bound at all: the refusal
    // has to leave the session exactly as it was.
    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.records, 2);
}

async fn a_refused_first_append_does_not_create_the_session(backend: Backend) {
    let fixture = backend.bounded(0);
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    let error = store.append(&session, say("one"), &cx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(error.to_string().contains("full"), "{error}");

    // The persistent store aborts its transaction and leaves nothing; the in-memory one must
    // not leave an empty session behind either. A refused append that quietly claimed the
    // session would hand its ownership to whoever was refused first.
    assert!(
        store.stats(&session, &cx).await.unwrap().is_none(),
        "a session nothing was ever appended to must not exist"
    );
    assert_eq!(store.clear(&session, &cx).await.unwrap(), 0);
    assert_eq!(
        store
            .window(&session, &ContextBudget::UNLIMITED, &cx)
            .await
            .unwrap(),
        ContextWindow::empty()
    );
}

async fn clearing_removes_the_session(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    let record = store.append(&session, say("one"), &cx).await.unwrap();
    assert_eq!(store.clear(&session, &cx).await.unwrap(), 1);
    assert!(
        store
            .get(&session, &record.id, &cx)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.stats(&session, &cx).await.unwrap().is_none());
    assert_eq!(
        store
            .window(&session, &ContextBudget::UNLIMITED, &cx)
            .await
            .unwrap(),
        ContextWindow::empty()
    );
}

async fn a_cleared_session_can_be_reclaimed_by_a_different_principal(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();

    store
        .append(&session, say("one"), &user("alice"))
        .await
        .unwrap();
    store.clear(&session, &user("alice")).await.unwrap();

    let record = store
        .append(&session, say("one"), &user("bob"))
        .await
        .unwrap();
    assert_eq!(record.principal, PrincipalId::new("bob"));
    // Clearing must not leave the old session's sequence behind: the reclaimed session is a
    // new one, not a continuation of a transcript its owner cannot read.
    assert_eq!(record.sequence, 0);
}

async fn an_unknown_session_has_no_stats_and_an_empty_window(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    assert!(store.stats(&session, &cx).await.unwrap().is_none());
    assert_eq!(
        store
            .window(&session, &ContextBudget::UNLIMITED, &cx)
            .await
            .unwrap(),
        ContextWindow::empty()
    );
    assert_eq!(store.clear(&session, &cx).await.unwrap(), 0);
}

async fn stats_report_full_fidelity_totals(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();
    let cx = user("alice");

    let first = store.append(&session, say("one"), &cx).await.unwrap();
    let second = store.append(&session, say("two"), &cx).await.unwrap();

    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.session, session);
    assert_eq!(stats.records, 2);
    assert_eq!(stats.tokens, first.tokens + second.tokens);
    assert_eq!(stats.owner, PrincipalId::new("alice"));
    assert_eq!(stats.created_at, first.created_at);
    assert!(stats.updated_at >= second.created_at);
}
