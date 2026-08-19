//! The agent loop, exercised against real collaborators.
//!
//! Every test here drives [`AgentLoop`] through the [`Agent`] contract and then asserts on
//! what the *other* subsystems saw: which requests the model was sent, which questions the
//! policy engine was asked, what ended up in the transcript, which audit events were
//! published. That is the only way to check the properties that matter — that nothing
//! reaches a tool without a decision, and that nothing a model wrote becomes something the
//! kernel trusts.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_agent::{AGENT_ATTRIBUTE, AgentLoopSettings, SESSION_ATTRIBUTE};
use aik_api::agent::{Agent, AgentRequest, AgentUpdate};
use aik_api::audit::{AuthorizationPhase, InvocationOutcome, ToolInvoked};
use aik_api::context::{ContextBudget, ContextStore, ELISION_MARKER};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Role};
use aik_api::permission::{ActionId, Decision, PrincipalId};
use aik_api::tool::ToolName;
use aik_core::clock::{ManualClock, SharedClock, Timestamp};
use aik_core::{ErrorKind, event::EventStream};
use futures::StreamExt;
use serde_json::json;
use support::{
    Behaviour, FixedApprovals, Harness, ProbeTool, RecordingPolicy, Reply, ScriptedModel, call,
    offered, text_of, user,
};
use tokio_util::sync::CancellationToken;

fn request(text: &str, session: aik_api::agent::SessionId) -> AgentRequest {
    AgentRequest {
        session,
        input: vec![ContentPart::text(text)],
        context: json!({ "window": "not-trusted" }),
    }
}

/// Drains whatever a subscription has already buffered.
fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut events = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        events.push(envelope.payload);
    }
    events
}

// --- terminating ---------------------------------------------------------------------

#[tokio::test]
async fn a_turn_without_tool_calls_ends_the_run() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("the answer is 42")])).build();
    let cx = user("alice");

    let response = harness
        .agent
        .run(request("what is it?", harness.session), &cx)
        .await
        .unwrap();

    assert_eq!(response.session, harness.session);
    assert_eq!(harness.model.call_count(), 1);
    assert_eq!(response.output, vec![ContentPart::text("the answer is 42")]);
}

#[tokio::test]
async fn the_input_is_recorded_before_the_model_is_asked() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("hello")])).build();
    let cx = user("alice");

    harness
        .agent
        .run(request("hi there", harness.session), &cx)
        .await
        .unwrap();

    let sent = harness.model.request(0);
    assert_eq!(sent.messages.len(), 1);
    assert_eq!(sent.messages[0].role, Role::User);
    assert_eq!(text_of(&sent.messages[0]), "hi there");

    // And the assistant's own turn is recorded too, so the next run continues from it.
    let transcript = harness.transcript(&cx).await;
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[1].message.role, Role::Assistant);
}

#[tokio::test]
async fn a_system_prompt_is_pinned_and_appended_once_per_session() {
    let settings = AgentLoopSettings {
        system_prompt: Some("you are terse".into()),
        ..AgentLoopSettings::new("test-model")
    };
    let harness = Harness::builder(ScriptedModel::new([
        Reply::answer("one"),
        Reply::answer("two"),
    ]))
    .settings(settings)
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("first", harness.session), &cx)
        .await
        .unwrap();
    harness
        .agent
        .run(request("second", harness.session), &cx)
        .await
        .unwrap();

    let transcript = harness.transcript(&cx).await;
    let prompts: Vec<_> = transcript
        .iter()
        .filter(|record| record.message.role == Role::System)
        .collect();
    assert_eq!(prompts.len(), 1, "one system prompt per session");
    assert!(prompts[0].pinned, "the prompt must survive eviction");
    assert!(
        transcript
            .iter()
            .filter(|record| record.message.role != Role::System)
            .all(|record| !record.pinned),
        "nothing a model produced may be pinned",
    );
}

#[tokio::test]
async fn usage_is_summed_across_every_turn() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]).costing(10, 3),
        Reply::answer("done").costing(20, 5),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();

    let response = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    let usage = response.usage.expect("both turns reported usage");
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.output_tokens, 8);
}

#[tokio::test]
async fn usage_is_absent_when_no_provider_reports_it() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("done")])).build();

    let response = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    assert!(response.usage.is_none());
}

// --- measurement -----------------------------------------------------------------------

#[tokio::test]
async fn a_measurement_event_is_published_once_per_turn_with_no_tools_offered() {
    let harness =
        Harness::builder(ScriptedModel::new([Reply::answer("done").costing(15, 4)])).build();
    let mut measurements = harness
        .events
        .subscribe::<aik_api::measurement::RequestMeasured>();

    harness
        .agent
        .run(request("hi", harness.session), &user("alice"))
        .await
        .unwrap();

    let measured = drain(&mut measurements);
    assert_eq!(measured.len(), 1);
    let event = &measured[0];
    assert_eq!(event.turn, 1);
    assert_eq!(event.estimate.tools_offered, 0);
    assert_eq!(event.estimate.tool_definition_tokens, 0);
    assert!(event.estimate.user_input_tokens.is_some());
    assert_eq!(
        event.provider_usage,
        Some(aik_api::model::Usage {
            input_tokens: 15,
            output_tokens: 4
        })
    );
}

#[tokio::test]
async fn tool_definitions_are_counted_even_when_none_are_called() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("done")]))
        .tool(ProbeTool::new("probe", Behaviour::Echo))
        .build();
    let mut measurements = harness
        .events
        .subscribe::<aik_api::measurement::RequestMeasured>();

    harness
        .agent
        .run(request("hi", harness.session), &user("alice"))
        .await
        .unwrap();

    let measured = drain(&mut measurements);
    assert_eq!(measured.len(), 1);
    assert_eq!(measured[0].estimate.tools_offered, 1);
    assert!(
        measured[0].estimate.tool_definition_tokens > 0,
        "a tool schema is never free",
    );
}

#[tokio::test]
async fn user_input_tokens_are_only_reported_on_the_first_turn_of_a_run() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();
    let mut measurements = harness
        .events
        .subscribe::<aik_api::measurement::RequestMeasured>();

    harness
        .agent
        .run(request("hi", harness.session), &user("alice"))
        .await
        .unwrap();

    let measured = drain(&mut measurements);
    assert_eq!(measured.len(), 2);
    assert!(measured[0].estimate.user_input_tokens.is_some());
    assert!(
        measured[1].estimate.user_input_tokens.is_none(),
        "the second turn carries no fresh user text",
    );
    // The second turn's conversation includes the tool call and its result.
    assert!(measured[1].estimate.tool_call_tokens > 0);
    assert!(measured[1].estimate.tool_result_tokens > 0);
}

#[tokio::test]
async fn cumulative_provider_usage_accumulates_across_turns() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]).costing(10, 3),
        Reply::answer("done").costing(20, 5),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();
    let mut measurements = harness
        .events
        .subscribe::<aik_api::measurement::RequestMeasured>();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    let measured = drain(&mut measurements);
    assert_eq!(measured.len(), 2);
    assert_eq!(
        measured[0].cumulative_provider_usage,
        Some(aik_api::model::Usage {
            input_tokens: 10,
            output_tokens: 3
        })
    );
    assert_eq!(
        measured[1].cumulative_provider_usage,
        Some(aik_api::model::Usage {
            input_tokens: 30,
            output_tokens: 8
        })
    );
}

#[tokio::test]
async fn no_measurement_is_published_without_an_event_bus_configured() {
    // Wiring an agent loop without `.with_events` must not fail or change turn behaviour —
    // only stop publishing. Built directly rather than through the harness, which always
    // wires events, to exercise the actual default.
    use aik_agent::AgentLoop;
    use aik_api::agent::Agent as _;

    let model = Arc::new(ScriptedModel::new([Reply::answer("done")]));
    let store = Arc::new(aik_context::InMemoryContextStore::new());
    let mut registry = aik_tools::InProcessToolRegistry::new();
    registry
        .register(ProbeTool::new("probe", Behaviour::Echo))
        .unwrap();
    let agent = AgentLoop::new(
        "no-events",
        model as Arc<dyn aik_api::model::ModelProvider>,
        Arc::new(registry),
        store,
        AgentLoopSettings::new("test-model"),
    );

    let response = agent
        .run(
            request("hi", aik_api::agent::SessionId::new()),
            &user("alice"),
        )
        .await
        .unwrap();
    assert_eq!(response.output, vec![ContentPart::text("done")]);
}

// --- tool calls ----------------------------------------------------------------------

#[tokio::test]
async fn a_tool_call_runs_through_the_registry_and_its_result_is_recorded() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({ "path": "notes.md" }))]),
        Reply::answer("it says hello"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();
    let cx = user("alice");

    let response = harness
        .agent
        .run(request("read it", harness.session), &cx)
        .await
        .unwrap();
    assert_eq!(response.output, vec![ContentPart::text("it says hello")]);

    // The tool was authorized before it ran, at capability level.
    let questions = harness.policy.questions();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].action, ActionId::new("probe"));
    assert_eq!(questions[0].principal.id, PrincipalId::new("alice"));

    // Call and result are both in the transcript, in that order.
    let transcript = harness.transcript(&cx).await;
    let roles: Vec<Role> = transcript
        .iter()
        .map(|record| record.message.role)
        .collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );

    let ContentPart::ToolResult {
        call_id,
        content,
        is_error,
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert_eq!(call_id, "c1");
    assert!(!is_error);
    assert_eq!(content["echo"], json!({ "path": "notes.md" }));

    // And the second turn saw the whole exchange.
    let second = harness.model.request(1);
    assert_eq!(second.messages.len(), 3);
}

#[tokio::test]
async fn every_tool_call_in_one_turn_runs_in_order() {
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([
            call("c1", "probe", json!({ "n": 1 })),
            call("c2", "probe", json!({ "n": 2 })),
            call("c3", "probe", json!({ "n": 3 })),
        ]),
        Reply::answer("all done"),
    ]))
    .tool(probe)
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let observed: Vec<i64> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|probe| probe.arguments["n"].as_i64().expect("a number"))
        .collect();
    assert_eq!(observed, vec![1, 2, 3]);

    let transcript = harness.transcript(&cx).await;
    let results: Vec<String> = transcript
        .iter()
        .flat_map(|record| record.message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["c1", "c2", "c3"]);
}

#[tokio::test]
async fn a_turn_can_both_speak_and_call_a_tool() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::saying("let me look", [call("c1", "probe", json!({}))]),
        Reply::answer("found it"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();

    let updates: Vec<AgentUpdate> = harness
        .agent
        .stream(request("go", harness.session), &user("alice"))
        .await
        .unwrap()
        .map(|update| update.unwrap())
        .collect()
        .await;

    assert!(
        matches!(&updates[0], AgentUpdate::Content(part) if *part == ContentPart::text("let me look"))
    );
    assert!(matches!(&updates[1], AgentUpdate::ToolCall(_)));
    assert!(matches!(&updates[2], AgentUpdate::ToolResult { .. }));
}

// --- authorization -------------------------------------------------------------------

#[tokio::test]
async fn a_denied_tool_is_reported_to_the_model_and_the_run_continues() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("understood, I cannot do that"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Forbidden))
    .policy(RecordingPolicy::denying("outside the workspace"))
    .build();
    let cx = user("alice");
    let mut invocations = harness.events.subscribe::<ToolInvoked>();

    let response = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();
    assert_eq!(
        response.output,
        vec![ContentPart::text("understood, I cannot do that")]
    );

    // The tool never ran — `Behaviour::Forbidden` would have panicked — and the model was
    // told why, in terms it can act on.
    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        content, is_error, ..
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(is_error);
    assert_eq!(content["kind"], json!("permission"));
    assert!(
        content["message"]
            .as_str()
            .unwrap()
            .contains("outside the workspace"),
        "{content}",
    );

    // The refusal is in the audit trail, not merely in the conversation.
    let events = drain(&mut invocations);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, InvocationOutcome::Denied);
}

#[tokio::test]
async fn the_same_tool_asked_for_twice_is_authorized_twice() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::calls([call("c2", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    assert_eq!(
        harness.policy.questions().len(),
        2,
        "no decision is cached across calls",
    );
}

#[tokio::test]
async fn resource_level_authorization_still_gates_the_call() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new(
        "probe",
        Behaviour::Claiming(ActionId::new("probe"), "/etc/shadow".into()),
    ))
    .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    let questions = harness.policy.questions();
    assert_eq!(questions.len(), 2, "capability, then resource");
    assert_eq!(questions[0].resource, None);
    assert_eq!(
        questions[1].resource.as_ref().map(|id| id.to_string()),
        Some("/etc/shadow".to_owned()),
    );
}

#[tokio::test]
async fn an_approval_the_human_refuses_becomes_an_error_result() {
    let approvals = Arc::new(FixedApprovals::refusing());
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("fine"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Forbidden))
    .policy(RecordingPolicy::allowing().deciding("probe", Decision::ask("run probe?")))
    .approvals(approvals.clone())
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    assert_eq!(approvals.asked(), 1);
    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        content, is_error, ..
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(is_error);
    assert_eq!(content["kind"], json!("permission"));
}

#[tokio::test]
async fn an_approval_the_human_grants_lets_the_tool_run() {
    let approvals = Arc::new(FixedApprovals::granting());
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({ "ok": true }))]),
        Reply::answer("done"),
    ]))
    .tool(probe)
    .policy(RecordingPolicy::allowing().deciding("probe", Decision::ask("run probe?")))
    .approvals(approvals.clone())
    .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    assert_eq!(approvals.asked(), 1);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_tool_outside_the_agents_set_never_reaches_the_registry() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "secret", json!({}))]),
        Reply::answer("understood"),
    ]))
    .tool(ProbeTool::new("allowed", Behaviour::Echo))
    .tool(ProbeTool::new("secret", Behaviour::Forbidden))
    .restricted_to([ToolName::new("allowed")])
    .build();
    let cx = user("alice");
    let mut invocations = harness.events.subscribe::<ToolInvoked>();

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    // Not offered to the model, not asked about, not invoked.
    assert_eq!(offered(&harness.model.request(0)), ["allowed"]);
    assert!(harness.policy.questions().is_empty());
    assert!(drain(&mut invocations).is_empty());

    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        content, is_error, ..
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(is_error);
    assert_eq!(content["kind"], json!("notfound"));
}

#[tokio::test]
async fn an_agents_tool_set_cannot_widen_what_the_registry_holds() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("nothing to do")]))
        .tool(ProbeTool::new("allowed", Behaviour::Echo))
        .restricted_to([ToolName::new("allowed"), ToolName::new("imaginary")])
        .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    assert_eq!(
        offered(&harness.model.request(0)),
        ["allowed"],
        "a name the registry does not have stays absent",
    );
}

// --- failures ------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_tool_is_reported_without_ending_the_run() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "ghost", json!({}))]),
        Reply::answer("my mistake"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();
    let cx = user("alice");

    let response = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();
    assert_eq!(response.output, vec![ContentPart::text("my mistake")]);

    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        call_id,
        content,
        is_error,
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert_eq!(call_id, "c1");
    assert!(is_error);
    assert_eq!(content["kind"], json!("notfound"));
}

#[tokio::test]
async fn a_tool_that_cannot_run_is_reported_to_the_model() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("I will try something else"),
    ]))
    .tool(ProbeTool::new(
        "probe",
        Behaviour::Fail("the disk is on fire".into()),
    ))
    .build();
    let cx = user("alice");
    let mut invocations = harness.events.subscribe::<ToolInvoked>();

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        content, is_error, ..
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(is_error);
    assert_eq!(content["kind"], json!("other"));

    let events = drain(&mut invocations);
    assert_eq!(
        events[0].outcome,
        InvocationOutcome::Failed {
            kind: "other".into()
        },
    );
}

#[tokio::test]
async fn a_tool_that_reports_a_failure_keeps_the_flag_the_tool_set() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("noted"),
    ]))
    .tool(ProbeTool::new(
        "probe",
        Behaviour::ReportError("no such file".into()),
    ))
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult {
        content, is_error, ..
    } = &transcript[2].message.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(is_error, "an invocation that succeeded but failed");
    assert_eq!(content["reason"], json!("no such file"));
}

#[tokio::test]
async fn a_model_that_fails_ends_the_run() {
    let harness =
        Harness::builder(ScriptedModel::new([Reply::failure("the provider is down")])).build();

    let error = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(error.to_string().contains("provider is down"), "{error}");
}

// --- cancellation and deadlines ------------------------------------------------------

#[tokio::test]
async fn an_already_cancelled_context_stops_before_the_model_is_called() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("never")])).build();
    let cx = user("alice");
    cx.cancellation.cancel();

    let error = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(harness.model.call_count(), 0);
    assert!(harness.transcript(&cx).await.is_empty());
}

#[tokio::test]
async fn cancelling_during_a_tool_call_stops_the_run() {
    let token = CancellationToken::new();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("never reached"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Cancel(token.clone())))
    .build();

    let cx = ExecutionContext {
        cancellation: token,
        ..user("alice")
    };

    let error = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(
        harness.model.call_count(),
        1,
        "no further turn once the run is cancelled",
    );
}

#[tokio::test]
async fn a_run_cancelled_during_a_turn_never_starts_the_tool_it_asked_for() {
    let token = CancellationToken::new();
    let cancelling = token.clone();
    let harness = Harness::builder(
        ScriptedModel::new([
            Reply::calls([call("c1", "probe", json!({}))]),
            Reply::answer("never reached"),
        ])
        .on_call(move |_| cancelling.cancel()),
    )
    .tool(ProbeTool::new("probe", Behaviour::Forbidden))
    .build();

    let cx = ExecutionContext {
        cancellation: token,
        ..user("alice")
    };

    let error = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap_err();

    // The turn itself completed and was recorded; the tool it asked for never ran, because
    // the check between deciding and doing saw the cancellation.
    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(harness.model.call_count(), 1);

    // The call it will now never make is closed off, so the session stays resumable.
    let transcript = harness.transcript(&cx).await;
    assert_eq!(transcript.len(), 3);
    let ContentPart::ToolResult {
        call_id,
        content,
        is_error,
    } = &transcript[2].message.content[0]
    else {
        panic!("expected the abandoned call to be answered");
    };
    assert_eq!(call_id, "c1");
    assert!(is_error);
    assert_eq!(content["kind"], json!("cancelled"));
}

#[tokio::test]
async fn a_run_that_stops_leaves_no_tool_call_unanswered() {
    // Two calls, a budget for none of them: whatever the reason a run stops, the transcript
    // must not end with an assistant turn that asks for tools nobody answered — most
    // providers reject exactly that when the session is resumed.
    let settings = AgentLoopSettings {
        max_tool_calls: 0,
        ..AgentLoopSettings::new("test-model")
    };
    let harness = Harness::builder(ScriptedModel::new([Reply::calls([
        call("c1", "probe", json!({})),
        call("c2", "probe", json!({})),
    ])]))
    .tool(ProbeTool::new("probe", Behaviour::Forbidden))
    .settings(settings)
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap_err();

    let transcript = harness.transcript(&cx).await;
    let answered: Vec<String> = transcript
        .iter()
        .flat_map(|record| record.message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(answered, vec!["c1", "c2"]);

    // And what the store holds is a window a provider would accept.
    let window = harness
        .store
        .window(&harness.session, &ContextBudget::UNLIMITED, &cx)
        .await
        .unwrap();
    let requested: Vec<&str> = window
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(requested, vec!["c1", "c2"]);
}

#[tokio::test]
async fn a_provider_that_reports_cancellation_ends_the_run() {
    let harness = Harness::builder(ScriptedModel::new([Reply::cancelled()])).build();

    let error = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Cancelled);
}

#[tokio::test]
async fn a_deadline_that_passes_mid_run_stops_it() {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let shared: SharedClock = clock.clone();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("never reached"),
    ]))
    .tool(ProbeTool::new(
        "probe",
        Behaviour::Advance(clock.clone(), Duration::from_millis(5_000)),
    ))
    .clock(shared)
    .build();

    let cx = user("alice").with_deadline(Timestamp::from_millis(1_000));

    let error = harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Timeout);
    assert_eq!(harness.model.call_count(), 1);
}

#[tokio::test]
async fn the_deadline_reaches_the_tool_unchanged() {
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(probe)
    .build();

    let deadline = Timestamp::now().saturating_add(Duration::from_secs(60));
    let cx = user("alice").with_deadline(deadline);

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let observed = seen.lock().unwrap();
    assert_eq!(observed[0].deadline, Some(deadline));
    assert_eq!(observed[0].correlation, cx.correlation);
    assert_eq!(
        observed[0].principal.as_ref().map(|p| p.id.clone()),
        Some(PrincipalId::new("alice")),
    );
}

// --- bounds --------------------------------------------------------------------------

#[tokio::test]
async fn a_model_that_never_stops_calling_tools_hits_the_turn_limit() {
    let settings = AgentLoopSettings {
        max_turns: 3,
        ..AgentLoopSettings::new("test-model")
    };
    let harness = Harness::builder(
        ScriptedModel::new([Reply::calls([call("c1", "probe", json!({}))])]).repeating(),
    )
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .settings(settings)
    .build();

    let error = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap_err();

    assert_eq!(harness.model.call_count(), 3);
    assert!(error.to_string().contains("3 model turns"), "{error}");
}

#[tokio::test]
async fn one_turn_asking_for_too_many_tools_hits_the_tool_call_limit() {
    let settings = AgentLoopSettings {
        max_tool_calls: 2,
        ..AgentLoopSettings::new("test-model")
    };
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let harness = Harness::builder(ScriptedModel::new([Reply::calls([
        call("c1", "probe", json!({})),
        call("c2", "probe", json!({})),
        call("c3", "probe", json!({})),
    ])]))
    .tool(probe)
    .settings(settings)
    .build();

    let error = harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap_err();

    assert_eq!(seen.lock().unwrap().len(), 2, "the third never ran");
    assert!(error.to_string().contains("2 tool calls"), "{error}");
}

// --- context budgets -----------------------------------------------------------------

#[tokio::test]
async fn the_window_is_reassembled_under_the_budget_every_turn() {
    let settings = AgentLoopSettings {
        budget: ContextBudget::UNLIMITED.with_max_part_tokens(8),
        ..AgentLoopSettings::new("test-model")
    };
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("read it"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Bulk(4_000)))
    .settings(settings)
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    // The window grew turn on turn, and the oversized result was elided on the way out.
    assert_eq!(harness.model.request(0).messages.len(), 1);
    let second = harness.model.request(1);
    assert_eq!(second.messages.len(), 3);

    let ContentPart::ToolResult { content, .. } = &second.messages[2].content[0] else {
        panic!("expected a tool result in the window");
    };
    assert!(content.get(ELISION_MARKER).is_some(), "{content}");
    assert!(!content.to_string().contains("xxxx"));

    // Nothing was lost: the store still holds the payload at full fidelity.
    let transcript = harness.transcript(&cx).await;
    let ContentPart::ToolResult { content, .. } = &transcript[2].message.content[0] else {
        panic!("expected a tool result in the transcript");
    };
    assert_eq!(content["body"].as_str().unwrap().len(), 4_000);
}

#[tokio::test]
async fn a_window_that_evicts_the_oldest_turns_still_reaches_the_model() {
    let settings = AgentLoopSettings {
        budget: ContextBudget::default().with_max_records(2),
        ..AgentLoopSettings::new("test-model")
    };
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .settings(settings)
    .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    let second = harness.model.request(1);
    assert!(
        second.messages.len() <= 2,
        "the record budget bounds the window: {}",
        second.messages.len(),
    );
}

// --- isolation and trust -------------------------------------------------------------

#[tokio::test]
async fn a_run_cannot_write_into_another_principals_session() {
    let harness = Harness::builder(ScriptedModel::new([Reply::answer("hello")])).build();

    harness
        .agent
        .run(request("mine", harness.session), &user("alice"))
        .await
        .unwrap();

    let error = harness
        .agent
        .run(request("yours", harness.session), &user("mallory"))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Permission);
    assert_eq!(
        harness.model.call_count(),
        1,
        "the second run never reached the model",
    );
}

#[tokio::test]
async fn records_are_attributed_to_the_runs_principal_not_to_the_model() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let transcript = harness.transcript(&cx).await;
    assert!(
        transcript
            .iter()
            .all(|record| record.principal == PrincipalId::new("alice")),
    );
    let sequences: Vec<u64> = transcript.iter().map(|record| record.sequence).collect();
    assert_eq!(sequences, vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn the_execution_context_a_tool_sees_carries_only_trusted_annotations() {
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(probe)
    .build();
    let cx = user("alice");

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let observed = seen.lock().unwrap();
    let attributes = &observed[0].attributes;
    assert_eq!(attributes[AGENT_ATTRIBUTE], json!("test.agent"));
    assert_eq!(
        attributes[SESSION_ATTRIBUTE],
        json!(harness.session.to_string())
    );
    assert_eq!(
        attributes.len(),
        2,
        "the caller's `AgentRequest::context` must not travel with authorization: {attributes:?}",
    );
}

#[tokio::test]
async fn tool_arguments_reach_the_tool_exactly_as_the_model_wrote_them() {
    let probe = ProbeTool::new("probe", Behaviour::Echo);
    let seen = probe.observations();
    let arguments = json!({ "path": "../../etc/shadow", "nested": { "n": [1, 2] } });
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", arguments.clone())]),
        Reply::answer("done"),
    ]))
    .tool(probe)
    .build();

    harness
        .agent
        .run(request("go", harness.session), &user("alice"))
        .await
        .unwrap();

    assert_eq!(
        seen.lock().unwrap()[0].arguments,
        arguments,
        "the loop neither interprets nor rewrites arguments",
    );
}

// --- streaming -----------------------------------------------------------------------

#[tokio::test]
async fn the_stream_ends_after_the_final_response() {
    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();

    let updates: Vec<AgentUpdate> = harness
        .agent
        .stream(request("go", harness.session), &user("alice"))
        .await
        .unwrap()
        .map(|update| update.unwrap())
        .collect()
        .await;

    assert!(matches!(updates[0], AgentUpdate::ToolCall(_)));
    assert!(matches!(updates[1], AgentUpdate::ToolResult { .. }));
    assert!(matches!(updates[2], AgentUpdate::Content(_)));
    assert!(matches!(updates[3], AgentUpdate::Finished(_)));
    assert_eq!(updates.len(), 4);
}

#[tokio::test]
async fn the_stream_reports_a_failure_and_then_ends() {
    let harness = Harness::builder(ScriptedModel::new([Reply::failure("down")])).build();

    let updates: Vec<_> = harness
        .agent
        .stream(request("go", harness.session), &user("alice"))
        .await
        .unwrap()
        .collect()
        .await;

    assert_eq!(updates.len(), 1);
    assert!(updates[0].is_err());
}

#[tokio::test]
async fn dropping_the_stream_stops_the_run() {
    let harness = Harness::builder(
        ScriptedModel::new([Reply::calls([call("c1", "probe", json!({}))])]).repeating(),
    )
    .tool(ProbeTool::new("probe", Behaviour::Echo))
    .build();

    let mut stream = harness
        .agent
        .stream(request("go", harness.session), &user("alice"))
        .await
        .unwrap();
    let first = stream.next().await.expect("an update").unwrap();
    assert!(matches!(first, AgentUpdate::ToolCall(_)));
    drop(stream);

    // The loop lives inside the stream, so nothing keeps turning once it is gone.
    tokio::task::yield_now().await;
    assert_eq!(harness.model.call_count(), 1);
}

// --- audit ---------------------------------------------------------------------------

#[tokio::test]
async fn every_phase_of_a_call_is_audited_under_one_correlation_id() {
    use aik_api::audit::AuthorizationDecided;

    let harness = Harness::builder(ScriptedModel::new([
        Reply::calls([call("c1", "probe", json!({}))]),
        Reply::answer("done"),
    ]))
    .tool(ProbeTool::new(
        "probe",
        Behaviour::Claiming(ActionId::new("probe"), "/tmp/x".into()),
    ))
    .build();
    let cx = user("alice");
    let mut decisions = harness.events.subscribe::<AuthorizationDecided>();
    let mut invocations = harness.events.subscribe::<ToolInvoked>();

    harness
        .agent
        .run(request("go", harness.session), &cx)
        .await
        .unwrap();

    let decisions = drain(&mut decisions);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decisions[1].phase, AuthorizationPhase::Resource);
    assert!(
        decisions
            .iter()
            .all(|event| event.correlation == cx.correlation),
        "every decision joins the caller's operation",
    );

    let invocations = drain(&mut invocations);
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].outcome, InvocationOutcome::Succeeded);
    assert_eq!(invocations[0].correlation, cx.correlation);
}
