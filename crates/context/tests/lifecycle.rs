//! Enumerating sessions, and compacting them.
//!
//! These are the two operations that turned a store you could only write into one you can
//! manage, and they fail in opposite directions. Enumeration fails by revealing too much: a
//! listing that named another principal's session would be a disclosure the ownership check
//! exists to prevent, and one that *errored* on encountering one would disclose the same
//! thing more quietly. Compaction fails by removing too much: a pinned system prompt that
//! disappeared, or a header whose counts no longer match the records behind it, is a session
//! that keeps working while being wrong.
//!
//! Every assertion runs against both implementations. The in-memory store is the reference —
//! whatever it does is the specification — and the durable one has to be indistinguishable
//! from it except for outliving the process.

use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextEntry, ContextStats};
use aik_api::execution::ExecutionContext;
use aik_api::model::{Message, Role};
use aik_api::permission::PrincipalId;
use aik_core::ErrorKind;
use aik_core::clock::{ManualClock, Timestamp};
use std::sync::Arc;

mod support;

use support::{Backend, agent_for, say, tool_exchange, user};

crate::both_backends!(
    a_store_with_no_sessions_enumerates_nothing,
    one_session_enumerates_with_its_totals,
    several_sessions_enumerate_newest_first,
    enumeration_shows_only_what_the_caller_owns,
    a_delegate_sees_the_sessions_it_acts_for,
    delegation_does_not_run_the_other_way_in_an_enumeration,
    enumeration_omits_rather_than_refuses_but_naming_one_still_refuses,
    an_enumeration_carries_no_transcript_content,
    compaction_removes_the_oldest_records_and_keeps_the_newest,
    compaction_never_removes_a_pinned_record,
    compaction_updates_the_record_and_token_counts,
    compacting_an_already_compacted_session_removes_nothing,
    compacting_a_session_with_room_to_spare_removes_nothing,
    compacting_to_zero_leaves_only_the_pinned_records,
    compacting_an_unknown_session_removes_nothing,
    another_principal_cannot_compact_a_session,
    a_full_session_becomes_appendable_again_after_compaction,
    compaction_does_not_reuse_sequence_numbers,
    compaction_leaves_no_stranded_tool_result_in_a_window,
    compaction_does_not_move_a_sessions_timestamps,
);

/// A pinned entry, which is what a system prompt is.
fn pinned(body: &str) -> ContextEntry {
    ContextEntry::new(Message::text(Role::System, body)).pinned()
}

/// The text of every message a window holds, in order.
fn texts(window: &aik_api::context::ContextWindow) -> Vec<String> {
    window
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            aik_api::model::ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The whole of a session, unbudgeted.
async fn whole(
    store: &std::sync::Arc<dyn aik_api::context::ContextStore>,
    session: &SessionId,
    cx: &ExecutionContext,
) -> Vec<String> {
    let window = store
        .window(session, &ContextBudget::UNLIMITED, cx)
        .await
        .unwrap();
    texts(&window)
}

async fn a_store_with_no_sessions_enumerates_nothing(backend: Backend) {
    let fixture = backend.open();
    assert_eq!(
        fixture.store().sessions(&user("alice")).await.unwrap(),
        Vec::<ContextStats>::new(),
        "an empty store is an empty list, not an error"
    );
}

async fn one_session_enumerates_with_its_totals(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    let first = store.append(&session, say("one"), &cx).await.unwrap();
    let second = store.append(&session, say("two"), &cx).await.unwrap();

    let listed = store.sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, session);
    assert_eq!(listed[0].owner, PrincipalId::new("alice"));
    assert_eq!(listed[0].records, 2);
    assert_eq!(listed[0].tokens, first.tokens + second.tokens);

    // The listing and the single-session read must agree; two ways of asking the same
    // question that could disagree would make neither trustworthy.
    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(listed[0], stats);
}

async fn several_sessions_enumerate_newest_first(backend: Backend) {
    // On a manual clock, because the assertion is about ordering by time: four appends on the
    // wall clock can easily share a millisecond, and a test that only passes when they do not
    // is asserting how fast the machine is.
    let fixture = backend.on_clock(Arc::new(ManualClock::new(Timestamp::from_millis(1_000))));
    let store = fixture.store();
    let cx = user("alice");
    let (first, second, third) = (SessionId::new(), SessionId::new(), SessionId::new());

    for session in [&first, &second, &third] {
        store.append(session, say("hello"), &cx).await.unwrap();
        fixture.advance(1_000);
    }
    // Touched last, so it must sort first whatever order the ids happen to have.
    store.append(&first, say("again"), &cx).await.unwrap();

    let listed = store.sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].session, first);

    let updated: Vec<_> = listed.iter().map(|stats| stats.updated_at).collect();
    let mut descending = updated.clone();
    descending.sort_by(|left, right| right.cmp(left));
    assert_eq!(updated, descending, "most recently updated first");

    // Deterministic under repetition, which the tie-break is what buys: two calls that see
    // the same sessions must agree about their order.
    assert_eq!(store.sessions(&cx).await.unwrap(), listed);
}

async fn enumeration_shows_only_what_the_caller_owns(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let (hers, his) = (SessionId::new(), SessionId::new());

    store
        .append(&hers, say("alice speaking"), &user("alice"))
        .await
        .unwrap();
    store
        .append(&his, say("bob speaking"), &user("bob"))
        .await
        .unwrap();

    let alice = store.sessions(&user("alice")).await.unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].session, hers);

    let bob = store.sessions(&user("bob")).await.unwrap();
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].session, his);

    // The system is an identity, not a master key — the same rule every other method here
    // applies, applied to enumeration.
    assert!(
        store
            .sessions(&ExecutionContext::new())
            .await
            .unwrap()
            .is_empty(),
        "the system principal must not enumerate everybody's conversations"
    );
}

async fn a_delegate_sees_the_sessions_it_acts_for(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let (hers, its_own) = (SessionId::new(), SessionId::new());

    store
        .append(&hers, say("alice speaking"), &user("alice"))
        .await
        .unwrap();
    store
        .append(
            &its_own,
            say("the agent's own"),
            &agent_for("assistant", "alice"),
        )
        .await
        .unwrap();

    let listed = store
        .sessions(&agent_for("assistant", "alice"))
        .await
        .unwrap();
    let ids: std::collections::BTreeSet<SessionId> =
        listed.iter().map(|stats| stats.session).collect();
    assert_eq!(
        ids,
        [hers, its_own].into_iter().collect(),
        "a delegate sees its own sessions and the ones it acts for"
    );
}

async fn delegation_does_not_run_the_other_way_in_an_enumeration(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let its_own = SessionId::new();

    store
        .append(
            &its_own,
            say("the agent's own"),
            &agent_for("assistant", "alice"),
        )
        .await
        .unwrap();

    // Acting for Alice does not make the agent's session Alice's, in a listing any more than
    // anywhere else.
    assert!(
        store.sessions(&user("alice")).await.unwrap().is_empty(),
        "delegation runs one way only"
    );
}

async fn enumeration_omits_rather_than_refuses_but_naming_one_still_refuses(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let hers = SessionId::new();

    store
        .append(&hers, say("alice speaking"), &user("alice"))
        .await
        .unwrap();

    // Mallory's listing succeeds and is empty. It must not error, because an error is itself
    // an answer: it would say a session exists that Mallory may not see.
    let listed = store.sessions(&user("mallory")).await.unwrap();
    assert!(listed.is_empty());

    // But naming the session — which requires already knowing the id — still fails closed on
    // every operation, including the two added with enumeration.
    for outcome in [
        store.stats(&hers, &user("mallory")).await.err(),
        store.clear(&hers, &user("mallory")).await.err(),
        store.compact(&hers, 0, &user("mallory")).await.err(),
        store
            .window(&hers, &ContextBudget::UNLIMITED, &user("mallory"))
            .await
            .err(),
    ] {
        let error = outcome.expect("naming another principal's session is refused");
        assert_eq!(error.kind(), ErrorKind::Permission, "{error}");
    }

    // And the refusals changed nothing.
    assert_eq!(
        store
            .stats(&hers, &user("alice"))
            .await
            .unwrap()
            .unwrap()
            .records,
        1
    );
}

async fn an_enumeration_carries_no_transcript_content(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    store
        .append(&session, say("the secret is hunter2"), &cx)
        .await
        .unwrap();

    // Serialised rather than field-by-field: the assertion is about the whole shape of what
    // enumeration hands back, so a field added later is covered without anyone remembering
    // to extend this test.
    let listed = store.sessions(&cx).await.unwrap();
    let json = serde_json::to_string(&listed).unwrap();
    assert!(
        !json.contains("hunter2"),
        "enumeration must report what a session costs, never what it says: {json}"
    );
}

async fn compaction_removes_the_oldest_records_and_keeps_the_newest(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three", "four", "five"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }

    assert_eq!(store.compact(&session, 2, &cx).await.unwrap(), 3);
    assert_eq!(whole(&store, &session, &cx).await, vec!["four", "five"]);
}

async fn compaction_never_removes_a_pinned_record(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    // Pinned records at the front, in the middle and at the very end, so a implementation
    // that only special-cases the first one is caught.
    store.append(&session, pinned("system"), &cx).await.unwrap();
    store.append(&session, say("one"), &cx).await.unwrap();
    store.append(&session, say("two"), &cx).await.unwrap();
    store.append(&session, pinned("task"), &cx).await.unwrap();
    store.append(&session, say("three"), &cx).await.unwrap();

    // Three unpinned records, keeping one: two go, both pinned records stay.
    assert_eq!(store.compact(&session, 1, &cx).await.unwrap(), 2);
    assert_eq!(
        whole(&store, &session, &cx).await,
        vec!["system", "task", "three"],
        "pinned records survive wherever they sit, and order is preserved"
    );
    assert_eq!(
        store.stats(&session, &cx).await.unwrap().unwrap().records,
        3
    );
}

async fn compaction_updates_the_record_and_token_counts(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    let mut appended = Vec::new();
    for body in ["one", "two", "three", "four"] {
        appended.push(store.append(&session, say(body), &cx).await.unwrap());
    }

    store.compact(&session, 1, &cx).await.unwrap();

    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.records, 1);
    assert_eq!(
        stats.tokens, appended[3].tokens,
        "the header must account for exactly what is left, not for what was ever there"
    );

    // The stored total and the recomputed one agree, which is the invariant the header
    // exists to make cheap and the one compaction could silently break.
    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    assert_eq!(window.usage.total_tokens(), stats.tokens);
    assert_eq!(window.usage.included_records, stats.records);
}

async fn compacting_an_already_compacted_session_removes_nothing(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }

    assert_eq!(store.compact(&session, 1, &cx).await.unwrap(), 2);
    let after = store.stats(&session, &cx).await.unwrap().unwrap();

    // Stable, rather than eroding a record each time it is asked: compaction is idempotent
    // for a fixed `keep`, which is what makes it safe to run on a timer or on every prompt.
    assert_eq!(store.compact(&session, 1, &cx).await.unwrap(), 0);
    assert_eq!(store.compact(&session, 1, &cx).await.unwrap(), 0);
    assert_eq!(store.stats(&session, &cx).await.unwrap().unwrap(), after);
}

async fn compacting_a_session_with_room_to_spare_removes_nothing(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    store.append(&session, say("one"), &cx).await.unwrap();
    store.append(&session, say("two"), &cx).await.unwrap();

    assert_eq!(store.compact(&session, 2, &cx).await.unwrap(), 0);
    assert_eq!(store.compact(&session, 99, &cx).await.unwrap(), 0);
    assert_eq!(whole(&store, &session, &cx).await, vec!["one", "two"]);
}

async fn compacting_to_zero_leaves_only_the_pinned_records(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    let system = store.append(&session, pinned("system"), &cx).await.unwrap();
    store.append(&session, say("one"), &cx).await.unwrap();
    store.append(&session, say("two"), &cx).await.unwrap();

    assert_eq!(store.compact(&session, 0, &cx).await.unwrap(), 2);
    assert_eq!(whole(&store, &session, &cx).await, vec!["system"]);

    let stats = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(stats.records, 1);
    assert_eq!(stats.tokens, system.tokens);

    // The pinned record is still addressable by id, which is the difference between
    // compacting a session and clearing it.
    assert!(
        store
            .get(&session, &system.id, &cx)
            .await
            .unwrap()
            .is_some()
    );
}

async fn compacting_an_unknown_session_removes_nothing(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    // The same answer `clear` gives, and for the same reason: there is nothing to refuse and
    // nothing to report, so saying so must not conjure the session either.
    assert_eq!(store.compact(&session, 5, &cx).await.unwrap(), 0);
    assert!(store.stats(&session, &cx).await.unwrap().is_none());
    assert!(store.sessions(&cx).await.unwrap().is_empty());
}

async fn another_principal_cannot_compact_a_session(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let session = SessionId::new();

    for body in ["one", "two", "three"] {
        store
            .append(&session, say(body), &user("alice"))
            .await
            .unwrap();
    }

    let error = store
        .compact(&session, 0, &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission, "{error}");
    assert!(error.to_string().contains("belongs to `alice`"), "{error}");

    // Refused *and* inert: a rejected compaction must not have removed anything on its way
    // to being rejected.
    assert_eq!(
        store
            .stats(&session, &user("alice"))
            .await
            .unwrap()
            .unwrap()
            .records,
        3
    );
}

async fn a_full_session_becomes_appendable_again_after_compaction(backend: Backend) {
    let fixture = backend.bounded(4);
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    store.append(&session, pinned("system"), &cx).await.unwrap();
    for body in ["one", "two", "three"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }

    // The state this whole operation exists to make recoverable.
    let full = store.append(&session, say("four"), &cx).await.unwrap_err();
    assert!(full.to_string().contains("full"), "{full}");

    assert_eq!(store.compact(&session, 1, &cx).await.unwrap(), 2);
    store.append(&session, say("four"), &cx).await.unwrap();

    assert_eq!(
        whole(&store, &session, &cx).await,
        vec!["system", "three", "four"]
    );
}

async fn compaction_does_not_reuse_sequence_numbers(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }
    store.compact(&session, 0, &cx).await.unwrap();

    // A transcript is append-only. Compaction shortens it from the front; it does not rewind
    // it, and a sequence number that came back would make two different records
    // indistinguishable in an audit.
    let next = store.append(&session, say("four"), &cx).await.unwrap();
    assert_eq!(next.sequence, 3);
}

async fn compaction_leaves_no_stranded_tool_result_in_a_window(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    let (call, result) = tool_exchange("c1", "the file's contents");
    store.append(&session, call, &cx).await.unwrap();
    store.append(&session, result, &cx).await.unwrap();
    store.append(&session, say("thanks"), &cx).await.unwrap();

    // Cuts between the call and the result it answers, which is the one case compaction can
    // produce that a provider would reject outright.
    assert_eq!(store.compact(&session, 2, &cx).await.unwrap(), 1);

    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    let orphaned = window.messages.iter().any(|message| {
        message.content.iter().any(|part| {
            matches!(part, aik_api::model::ContentPart::ToolResult { content, .. }
                if content.get("content").is_some())
        })
    });
    assert!(
        !orphaned,
        "a tool result whose call compaction removed must not reach a provider: {window:?}"
    );
}

async fn compaction_does_not_move_a_sessions_timestamps(backend: Backend) {
    let fixture = backend.open();
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }
    let before = store.stats(&session, &cx).await.unwrap().unwrap();

    store.compact(&session, 1, &cx).await.unwrap();
    let after = store.stats(&session, &cx).await.unwrap().unwrap();

    // Compaction is housekeeping, not activity. `updated_at` is the retention clock, and one
    // that housekeeping reset would guarantee a session never expired.
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.updated_at, before.updated_at);
}
