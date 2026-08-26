//! The compactor as a kernel component: resolved by capability, observable on the bus.

mod support;

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{
    ContextBudget, ContextCompacted, ContextCompactor, ContextEntry, ContextStore,
};
use aik_api::model::{Message, ModelProvider, Role};
use aik_context::ContextComponent;
use aik_core::prelude::*;
use aik_summary::{DEFAULT_COMPONENT_ID, SummaryComponent, SummarySettings};

use support::{ScriptedModel, alice};

/// Publishes a scripted model as the kernel's `dyn ModelProvider`.
struct ModelComponent(Arc<ScriptedModel>);

#[async_trait]
impl Component for ModelComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentId::new("model.stub"))
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn ModelProvider>(self.0.clone())
    }
}

async fn kernel(model: Arc<ScriptedModel>) -> Kernel {
    let kernel = Kernel::builder()
        .component(ModelComponent(model))
        .component(ContextComponent::new())
        .component(
            SummaryComponent::new(SummarySettings::new("small").keeping(4))
                .requires("context.store")
                .requires("model.stub"),
        )
        .build()
        .expect("a kernel");
    kernel.start().await.expect("a started kernel");
    kernel
}

#[tokio::test]
async fn the_component_publishes_a_compactor() {
    let kernel = kernel(ScriptedModel::saying("a recap")).await;
    assert!(
        kernel.context().service::<dyn ContextCompactor>().is_ok(),
        "the capability must be resolvable by anything that wants room"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn compaction_is_observable_without_being_readable() {
    let kernel = kernel(ScriptedModel::saying("they discussed the secret plan")).await;
    let ctx = kernel.context();
    let mut events = ctx.subscribe::<ContextCompacted>();
    let firehose = ctx.events().subscribe_any();

    let store = ctx.service::<dyn ContextStore>().unwrap();
    let compactor = ctx.service::<dyn ContextCompactor>().unwrap();
    let session = SessionId::new();
    let cx = alice();
    for index in 0..10 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(
                    Role::User,
                    format!("turn {index} about a distinctive secret phrase"),
                )),
                &cx,
            )
            .await
            .unwrap();
    }

    let compaction = compactor
        .compact(&session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();

    let envelope = events.recv().await.expect("a compaction event");
    assert_eq!(
        envelope.metadata.source.as_ref(),
        Some(&ComponentId::new(DEFAULT_COMPONENT_ID)),
        "compaction is attributed to the component that did it"
    );
    let event = envelope.payload;
    assert_eq!(event.session, session);
    assert_eq!(event.correlation, cx.correlation);
    assert_eq!(event.compaction, compaction);

    // The firehose is what a bridge or a log aggregator sees. Neither the conversation nor
    // the recap of it may be in there.
    let mut firehose = firehose;
    let mut saw_compaction = false;
    while let Some(Ok(envelope)) = firehose.try_recv() {
        let json = serde_json::to_string(&envelope.payload).unwrap();
        assert!(
            !json.contains("distinctive secret phrase"),
            "context events must not carry conversation content: {json}"
        );
        assert!(
            !json.contains("secret plan"),
            "and must not carry the recap either: {json}"
        );
        saw_compaction |= json.contains("summarised_records");
    }
    assert!(
        saw_compaction,
        "the compaction event should reach the firehose"
    );

    kernel.shutdown().await.unwrap();
}
