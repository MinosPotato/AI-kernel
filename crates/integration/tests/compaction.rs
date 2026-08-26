//! Summarising compaction over the durable stack.
//!
//! `aik-summary` is tested against an in-memory transcript in its own suite, and the durable
//! transcript is tested against the contract in `aik-context`'s. Neither can assert what this
//! file does, because it is a property of the seam rather than of either side: a compactor
//! reads through [`ContextStore`], writes through it, and reclaims through it, so what a
//! session looks like *afterwards* is decided by two crates and observable only when both are
//! real and the database is a file.
//!
//! What is pinned down here:
//!
//! 1. a recap outlives the process, and the turns it replaced do not come back with it;
//! 2. the store's own accounting — record count, token total, the timestamps retention runs
//!    off — stays consistent with what compaction left behind.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{
    ContextBudget, ContextCompactor, ContextEntry, ContextRecord, ContextStore, SUMMARY_MARKER,
};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Message, ModelProvider, Role};
use aik_context::RedbContextComponent;
use aik_core::prelude::*;
use aik_store::StoreComponent;
use aik_summary::{SummaryComponent, SummarySettings};

mod support;
use support::agent::{Reply, ScriptedModel};
use support::{store_config, user};

/// Publishes the scripted model, which is the only stub in this file.
struct StubModelComponent(Arc<ScriptedModel>);

#[async_trait]
impl Component for StubModelComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("model.stub").described("a scripted model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn ModelProvider>(self.0.clone())
    }
}

/// A kernel over `path` holding the durable transcript and a compactor above it.
async fn open(path: &std::path::Path, model: Arc<ScriptedModel>) -> Kernel {
    let kernel = Kernel::builder()
        .config(store_config(path))
        .component(StoreComponent::new())
        .component(RedbContextComponent::new())
        .component(StubModelComponent(model))
        .component(
            SummaryComponent::new(SummarySettings::new("test-model").keeping(4))
                .requires(aik_context::DEFAULT_COMPONENT_ID)
                .requires("model.stub"),
        )
        .build()
        .expect("a valid kernel");
    kernel.start().await.expect("the kernel starts");
    kernel
}

/// Every record of a session, oldest first, read back at full fidelity.
async fn records(
    store: &Arc<dyn ContextStore>,
    session: &SessionId,
    cx: &ExecutionContext,
) -> Vec<ContextRecord> {
    let window = store
        .window(session, &ContextBudget::UNLIMITED, cx)
        .await
        .expect("a window");
    let mut records = Vec::new();
    for id in window.records {
        records.push(
            store
                .get(session, &id, cx)
                .await
                .expect("a readable record")
                .expect("a record the window named"),
        );
    }
    records
}

/// The text of one record.
fn line(record: &ContextRecord) -> String {
    record
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of every record in a session, oldest first.
async fn transcript(
    store: &Arc<dyn ContextStore>,
    session: &SessionId,
    cx: &ExecutionContext,
) -> Vec<String> {
    records(store, session, cx).await.iter().map(line).collect()
}

#[tokio::test]
async fn a_recap_outlives_the_process_and_the_turns_it_replaced_do_not() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("aik.redb");
    let session = SessionId::new();
    let cx = user("alice");

    let model = ScriptedModel::new();
    model.script([Reply::answer("they counted from zero to nine")]);

    let kernel = open(&path, model).await;
    let store = kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a durable transcript");
    for index in 0..10 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("turn {index}"))),
                &cx,
            )
            .await
            .expect("a turn");
    }
    let before = store
        .stats(&session, &cx)
        .await
        .expect("readable")
        .expect("a session");

    let compaction = kernel
        .context()
        .service::<dyn ContextCompactor>()
        .expect("a compactor")
        .compact(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .expect("compaction");
    assert_eq!(compaction.summarised_records, 6);
    assert_eq!(compaction.removed_records, 6);

    kernel.shutdown().await.expect("a clean shutdown");
    // redb refuses to open one file twice, and the handle is held by the registry the kernel
    // owns and by every service resolved out of it. Both have to be gone before the second
    // kernel opens the same path — which is exactly the constraint a second *process* is
    // under, and the reason this test is worth having.
    drop(store);
    drop(kernel);

    // A second process, the same file. Nothing about what it finds may depend on the first
    // one still being around.
    let kernel = open(&path, ScriptedModel::new()).await;
    let store = kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a durable transcript");

    let lines = transcript(&store, &session, &cx).await;
    assert_eq!(lines.len(), 5, "four kept turns and the recap: {lines:?}");
    assert!(
        lines
            .last()
            .expect("a recap")
            .contains("they counted from zero to nine"),
        "{lines:?}"
    );
    assert!(lines.last().expect("a recap").contains(SUMMARY_MARKER));
    assert!(
        !lines.iter().any(|line| line == "turn 0"),
        "a summarised turn does not come back: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "turn 9"),
        "the recent end survives: {lines:?}"
    );

    // The store's own accounting has to describe what it now holds, not what it held before
    // a compactor rewrote the front of the session. Deliberately not "fewer tokens than
    // before": six one-word turns replaced by a labelled recap is a larger session and a
    // correct one, and a test that asserted otherwise would only hold for toy transcripts.
    let after = store
        .stats(&session, &cx)
        .await
        .expect("readable")
        .expect("a session");
    let held = records(&store, &session, &cx).await;
    assert_eq!(after.records, held.len());
    assert_eq!(
        after.tokens,
        held.iter().map(|record| record.tokens).sum::<u64>(),
        "stats must agree with the records that survived"
    );
    assert_eq!(
        after.created_at, before.created_at,
        "compaction is not a new session"
    );

    kernel.shutdown().await.expect("a clean shutdown");
}

#[tokio::test]
async fn a_pinned_prompt_and_someone_elses_session_are_both_left_alone() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("aik.redb");
    let alice = user("alice");
    let bob = user("bob");
    let hers = SessionId::new();
    let his = SessionId::new();

    let model = ScriptedModel::new();
    model.script([Reply::answer("a recap")]);
    let kernel = open(&path, model).await;
    let store = kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a durable transcript");

    store
        .append(
            &hers,
            ContextEntry::new(Message::text(Role::System, "you are careful")).pinned(),
            &alice,
        )
        .await
        .expect("a pinned prompt");
    for index in 0..10 {
        store
            .append(
                &hers,
                ContextEntry::new(Message::text(Role::User, format!("turn {index}"))),
                &alice,
            )
            .await
            .expect("a turn");
    }
    store
        .append(
            &his,
            ContextEntry::new(Message::text(Role::User, "bob's own conversation")),
            &bob,
        )
        .await
        .expect("bob's turn");

    let compactor = kernel
        .context()
        .service::<dyn ContextCompactor>()
        .expect("a compactor");

    // Bob cannot compact Alice's session, and finds out nothing by trying.
    let error = compactor
        .compact(&hers, &ContextBudget::UNLIMITED, &bob)
        .await
        .expect_err("a session bob does not own");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission);

    compactor
        .compact(&hers, &ContextBudget::UNLIMITED, &alice)
        .await
        .expect("alice's own session");

    let lines = transcript(&store, &hers, &alice).await;
    assert_eq!(
        lines[0], "you are careful",
        "a pinned prompt is never a turn"
    );
    assert_eq!(lines.len(), 6);
    assert_eq!(
        transcript(&store, &his, &bob).await,
        vec!["bob's own conversation".to_owned()],
        "compacting one session touches no other"
    );

    kernel.shutdown().await.expect("a clean shutdown");
}
