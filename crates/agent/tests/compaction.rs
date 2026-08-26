//! When the loop asks for room, and what happens when the answer is unhelpful.
//!
//! The loop contributes exactly one thing to summarisation: the trigger. So that is what is
//! tested here — *when* a compactor is asked, *how often*, and what the run does with each
//! way the answer can go wrong. What a recap actually says belongs to
//! [`aik-summary`](../aik_summary/index.html) and is tested there.

mod support;

use aik_agent::AgentLoopSettings;
use aik_api::agent::{Agent, AgentRequest, SessionId};
use aik_api::context::ContextBudget;
use aik_api::model::ContentPart;
use aik_core::ErrorKind;

use support::{Compacts, Harness, Reply, ScriptedModel, call, text_of, user};

fn request(text: &str, session: SessionId) -> AgentRequest {
    AgentRequest {
        session,
        input: vec![ContentPart::text(text)],
        context: serde_json::Value::Null,
    }
}

fn tight() -> AgentLoopSettings {
    let mut settings = AgentLoopSettings::new("test-model");
    settings.budget = ContextBudget::tokens(120);
    settings
}

#[tokio::test]
async fn a_session_that_fits_its_budget_is_never_compacted() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("hello")]))
        .compactor(Compacts::Nothing)
        .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("hi", harness.session), &cx)
        .await
        .expect("an answer");

    assert_eq!(
        harness.compactor.as_ref().expect("a compactor").calls(),
        0,
        "compaction costs a model call and buys nothing while the window still fits"
    );
}

#[tokio::test]
async fn an_overflowing_session_is_compacted_before_the_question_is_recorded() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("hello")]))
        .settings(tight())
        .compactor(Compacts::Recapping {
            text: "[recap] they discussed the earlier turns".into(),
            keep: 2,
        })
        .build();
    let cx = user("alice");
    harness.fill(12, 12, &cx).await;

    harness
        .agent
        .run(request("and what did I ask first?", harness.session), &cx)
        .await
        .expect("an answer");

    let compactor = harness.compactor.as_ref().expect("a compactor");
    assert_eq!(compactor.calls(), 1);
    assert_eq!(
        compactor.seen(),
        vec![12],
        "the recap must land behind the question, so the question is not recorded yet"
    );

    // What the model was shown: the recap, and no trace of the turns it replaced.
    let sent = harness.model.request(0);
    let window: Vec<String> = sent.messages.iter().map(text_of).collect();
    assert!(
        window
            .iter()
            .any(|text| text.contains("they discussed the earlier turns")),
        "{window:?}"
    );
    assert!(
        window
            .last()
            .expect("a message")
            .contains("what did I ask first"),
        "the question is the last thing in the window: {window:?}"
    );
    assert!(
        !window.iter().any(|text| text.contains("turn 0:")),
        "the summarised turns are gone: {window:?}"
    );
}

#[tokio::test]
async fn a_failing_compactor_does_not_fail_the_run() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("answered anyway")]))
        .settings(tight())
        .compactor(Compacts::Failure("the summarising model is down".into()))
        .build();
    let cx = user("alice");
    harness.fill(12, 12, &cx).await;

    let response = harness
        .agent
        .run(request("still there?", harness.session), &cx)
        .await
        .expect("a run that survives a failed compaction");

    assert!(
        matches!(&response.output[0], ContentPart::Text { text } if text == "answered anyway"),
        "{:?}",
        response.output
    );
    assert_eq!(
        harness.compactor.as_ref().expect("a compactor").calls(),
        1,
        "a failure disables further attempts rather than being retried every turn"
    );
}

#[tokio::test]
async fn a_compactor_with_nothing_to_do_is_asked_once_per_run() {
    // Three model turns, two of them driving a tool, all of them over budget: without the
    // latch this would be three fruitless compaction attempts.
    let harness = Harness::builder(
        ScriptedModel::new([
            Reply::calls([call("c1", "kernel.echo", serde_json::json!({ "a": 1 }))]),
            Reply::calls([call("c2", "kernel.echo", serde_json::json!({ "b": 2 }))]),
            Reply::answer("done"),
        ])
        .repeating(),
    )
    .tool(aik_tools::EchoTool::new())
    .settings(tight())
    .compactor(Compacts::Nothing)
    .build();
    let cx = user("alice");
    harness.fill(12, 12, &cx).await;

    harness
        .agent
        .run(request("run the tool twice", harness.session), &cx)
        .await
        .expect("an answer");

    assert_eq!(harness.compactor.as_ref().expect("a compactor").calls(), 1);
}

#[tokio::test]
async fn a_cancelled_compaction_stops_the_run() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("never reached")]))
        .settings(tight())
        .compactor(Compacts::Cancelled)
        .build();
    let cx = user("alice");
    harness.fill(12, 12, &cx).await;

    let error = harness
        .agent
        .run(request("hello", harness.session), &cx)
        .await
        .expect_err("a cancelled run does not continue");

    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(
        harness.model.call_count(),
        0,
        "the turn must not be taken after the run was cancelled"
    );
}

#[tokio::test]
async fn a_loop_with_no_compactor_behaves_exactly_as_it_did() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("hello")]))
        .settings(tight())
        .build();
    let cx = user("alice");
    harness.fill(12, 12, &cx).await;

    harness
        .agent
        .run(request("hi", harness.session), &cx)
        .await
        .expect("an answer");

    assert!(harness.compactor.is_none());
    let sent = harness.model.request(0);
    assert!(
        sent.messages.len() < 13,
        "the budget still evicts on its own: {} messages",
        sent.messages.len()
    );
}
