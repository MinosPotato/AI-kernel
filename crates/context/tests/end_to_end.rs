//! The context layer as an agent loop will actually use it: through the kernel, over many
//! turns, with real tool output in the transcript.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{ContextAssembled, ContextBudget, ContextEntry, ContextStore, TokenCounter};
use aik_api::execution::ExecutionContext;
use aik_api::model::{Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_context::{ContextComponent, HeuristicTokenCounter, RedbContextComponent};
use aik_core::ErrorKind;
use aik_core::prelude::*;
use aik_store::{Db, StoreComponent};
use serde_json::json;

fn config_for(path: &std::path::Path) -> Config {
    Config::builder()
        .layer(json!({
            "components": { "store": { "db": { "path": path } } }
        }))
        .build()
}

fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
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

#[tokio::test]
async fn the_persistent_component_writes_to_the_kernel_s_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbContextComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let store = ctx.service::<dyn ContextStore>().unwrap();
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
    let window = store
        .window(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    assert_eq!(window.usage.included_records, 1);

    // The transcript is in the kernel's database, not a second file of the context store's
    // own: one file to secure, back up or delete.
    let db = ctx.service::<Db>().unwrap();
    assert_eq!(db.path(), path);

    // Everything holding the database has to go before it can be reopened: redb's exclusive
    // lock means a second open while the first kernel is alive fails, which is exactly the
    // guarantee that stops two kernels writing one transcript.
    kernel.shutdown().await.unwrap();
    drop((db, store, ctx, kernel));

    // Reopening the same file finds the same record, which is the whole reason to prefer
    // this component over the in-memory one.
    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbContextComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let store = kernel.context().service::<dyn ContextStore>().unwrap();
    let found = store
        .get(&session, &record.id, &cx)
        .await
        .unwrap()
        .expect("the record survived the restart");
    assert_eq!(found, record);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_persistent_component_refuses_to_start_without_a_database() {
    // Better a startup failure attributed to the missing dependency than a kernel that
    // comes up with a context store nobody notices is absent.
    let error = Kernel::builder()
        .component(RedbContextComponent::new())
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Wiring);
    assert!(
        error.to_string().contains("store.db"),
        "the failure should name the database component, got `{error}`"
    );
}
