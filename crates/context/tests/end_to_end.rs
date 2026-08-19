//! The context layer as an agent loop will actually use it: through the kernel, over many
//! turns, with real tool output in the transcript.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{
    ContextAssembled, ContextBudget, ContextEntry, ContextStore, ELISION_MARKER, TokenCounter,
};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::tool::{ToolCall, ToolName};
use aik_context::{ContextComponent, HeuristicTokenCounter, InMemoryContextStore};
use aik_core::prelude::*;
use serde_json::json;

fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
}

/// One assistant turn that calls a tool, and the tool result that answers it.
fn tool_exchange(call_id: &str, payload: &str) -> (ContextEntry, ContextEntry) {
    let call = ContextEntry::new(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(ToolCall {
            call_id: call_id.into(),
            name: ToolName::new("filesystem.read"),
            arguments: json!({ "path": "src/lib.rs" }),
        })],
        name: None,
    });
    let result = ContextEntry::new(Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult {
            call_id: call_id.into(),
            content: json!({ "path": "src/lib.rs", "content": payload }),
            is_error: false,
        }],
        name: None,
    });
    (call, result)
}

#[tokio::test]
async fn the_component_publishes_a_store_and_a_counter() {
    let kernel = Kernel::builder()
        .component(ContextComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let store = ctx.service::<dyn ContextStore>().unwrap();
    let counter = ctx.service::<dyn TokenCounter>().unwrap();

    let session = SessionId::new();
    let cx = alice();
    let record = store
        .append(
            &session,
            ContextEntry::new(Message::text(Role::User, "hello")),
            &cx,
        )
        .await
        .unwrap();

    assert_eq!(
        record.tokens,
        counter.count_message(&Message::text(Role::User, "hello")),
        "the store must cost records with the same counter it publishes"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn assembling_a_window_publishes_usage_without_publishing_content() {
    let kernel = Kernel::builder()
        .component(ContextComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let mut events = ctx.subscribe::<ContextAssembled>();
    let firehose = ctx.events().subscribe_any();
    let store = ctx.service::<dyn ContextStore>().unwrap();

    let session = SessionId::new();
    let cx = alice();
    store
        .append(
            &session,
            ContextEntry::new(Message::text(Role::User, "a distinctive secret phrase")),
            &cx,
        )
        .await
        .unwrap();
    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();

    let event = events.recv().await.expect("an assembly event").payload;
    assert_eq!(event.session, session);
    assert_eq!(event.correlation, cx.correlation);
    assert_eq!(event.usage, window.usage);

    // The firehose is what a bridge or a log aggregator sees. Conversation content must not
    // be in it.
    let mut firehose = firehose;
    let mut saw_assembly = false;
    while let Some(Ok(envelope)) = firehose.try_recv() {
        let json = serde_json::to_string(&envelope.payload).unwrap();
        assert!(
            !json.contains("distinctive secret phrase"),
            "context events must not carry conversation content: {json}"
        );
        saw_assembly |= json.contains("included_records");
    }
    assert!(saw_assembly, "the assembly event should reach the firehose");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_long_conversation_costs_a_bounded_amount_per_turn() {
    let store = InMemoryContextStore::new();
    let session = SessionId::new();
    let cx = alice();

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

#[tokio::test]
async fn the_system_prompt_survives_every_turn() {
    let store = InMemoryContextStore::new();
    let session = SessionId::new();
    let cx = alice();
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

#[tokio::test]
async fn an_elided_tool_result_stays_retrievable_in_full() {
    let store = InMemoryContextStore::new();
    let session = SessionId::new();
    let cx = alice();
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

#[tokio::test]
async fn a_window_is_derived_not_stored() {
    let store = InMemoryContextStore::new();
    let session = SessionId::new();
    let cx = alice();

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

#[tokio::test]
async fn a_provider_specific_counter_replaces_the_default_everywhere() {
    /// Counts every byte as a token: four times the default estimate.
    struct Pessimist;

    impl TokenCounter for Pessimist {
        fn count_text(&self, text: &str) -> u64 {
            text.len() as u64
        }
    }

    let kernel = Kernel::builder()
        .component(ContextComponent::new().with_token_counter(Arc::new(Pessimist)))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let store = ctx.service::<dyn ContextStore>().unwrap();
    let session = SessionId::new();
    let cx = alice();

    let message = Message::text(Role::User, "abcdefghijklmnop");
    let record = store
        .append(&session, ContextEntry::new(message.clone()), &cx)
        .await
        .unwrap();

    assert_eq!(record.tokens, Pessimist.count_text("abcdefghijklmnop") + 4);
    assert!(record.tokens > HeuristicTokenCounter::new().count_message(&message));

    // And the registered counter is the same one, so a caller estimating before appending
    // gets the same answer the store will.
    let counter = ctx.service::<dyn TokenCounter>().unwrap();
    assert_eq!(counter.count_message(&message), record.tokens);

    kernel.shutdown().await.unwrap();
}
