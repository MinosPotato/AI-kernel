//! The boundaries the frontend is responsible for holding.
//!
//! Every test here starts the real wiring — the real registry, policy engine, broker, store
//! and agent loop — around a scripted model, and then asserts on what the *rest of the
//! system* saw: which principal asked, what the policy engine was told, which audit events
//! were published, and whether the operation actually happened. A frontend that got any of
//! this wrong would still look completely normal on screen, which is exactly why it is
//! asserted from the audit trail rather than from the output.

mod support;

use aik_api::audit::{AuthorizationDecided, AuthorizationOutcome, InvocationOutcome, ToolInvoked};
use aik_api::permission::PrincipalKind;
use aik_cli::args::ToolSet;
use aik_cli::console::Console;
use aik_cli::session::Session;
use aik_core::event::EventStream;
use serde_json::json;
use support::{HarnessBuilder, Reply};

/// Drains whatever a stream has buffered.
fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut events = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        events.push(envelope.payload);
    }
    events
}

fn root_with_secret() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(directory.path().join("notes.txt"), "the file's contents")
        .expect("a file to read");
    directory
}

fn read_call(path: &str) -> Reply {
    Reply::call("c1", "filesystem.read", json!({ "path": path }))
}

/// Reading is allowed as a capability, and every specific file is put to a human.
///
/// Two rules because a rule with no `resource` answers only the capability-level question
/// and a rule with `"*"` answers both; ordering them this way means exactly one prompt per
/// call, about the file, rather than one about the capability as well.
fn ask_per_file() -> serde_json::Value {
    json!([
        { "action": "filesystem.read", "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "resource": "*",
          "effect": { "decision": "require_approval", "prompt": "let it read?" } }
    ])
}

/// Reading anything, with no questions asked.
fn allow_reads() -> serde_json::Value {
    json!([
        { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ])
}

/// Everything, at both levels. Used where the point is that policy is *not* the limit.
fn allow_everything() -> serde_json::Value {
    json!([
        { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "*", "effect": { "decision": "allow" } }
    ])
}

// ---------------------------------------------------------------------------
// one-shot runs fail closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_one_shot_run_refuses_anything_a_policy_defers_to_a_human() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(ask_per_file())
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("I could not read it"))
        .build(root.path())
        .await;

    let mut decisions = harness.kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    // No console input at all: there is nobody there, which is the point.
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    let decisions = drain(&mut decisions);
    assert!(
        decisions
            .iter()
            .any(|decision| matches!(decision.outcome, AuthorizationOutcome::ApprovalUnavailable)),
        "a question nobody can answer must be recorded as unanswerable: {decisions:?}",
    );
    assert!(
        decisions
            .iter()
            .filter(|decision| decision.resource.is_some())
            .all(|decision| !decision.outcome.is_allowed()),
        "no decision about a specific file may be an allow: {decisions:?}",
    );
    assert_eq!(
        drain(&mut invocations)
            .into_iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        vec![InvocationOutcome::Denied],
    );

    // And the thing itself did not happen: the file's contents are nowhere in what the
    // model was subsequently sent.
    let sent = format!("{:?}", harness.model.requests());
    assert!(!sent.contains("the file's contents"), "{sent}");

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_one_shot_run_still_performs_what_a_policy_allows_outright() {
    // The refusal above must come from there being no responder, not from one-shot mode
    // being broken.
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(allow_reads())
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("it says something"))
        .build(root.path())
        .await;

    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    assert_eq!(
        drain(&mut invocations)
            .into_iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        vec![InvocationOutcome::Succeeded],
    );
    let sent = format!("{:?}", harness.model.requests());
    assert!(sent.contains("the file's contents"), "{sent}");

    harness.kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// interactive runs put the question to a person
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_interactive_session_grants_what_the_person_allows() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(ask_per_file())
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("it says something"))
        .build(root.path())
        .await;

    let mut decisions = harness.kernel.context().subscribe::<AuthorizationDecided>();

    // "y" answers the approval; "/quit" ends the session afterwards.
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"read notes.txt\ny\n/quit\n"[..]),
        Some(harness.broker.gate().subscribe()),
    )
    .expect("a session");
    session.interactive().await.unwrap();

    assert!(
        drain(&mut decisions)
            .iter()
            .any(|decision| matches!(decision.outcome, AuthorizationOutcome::ApprovalGranted)),
        "the person said yes, so the grant must be recorded as theirs",
    );
    let sent = format!("{:?}", harness.model.requests());
    assert!(sent.contains("the file's contents"), "{sent}");

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interactive_session_refuses_what_the_person_declines() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(ask_per_file())
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("I was not allowed"))
        .build(root.path())
        .await;

    let mut decisions = harness.kernel.context().subscribe::<AuthorizationDecided>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"read notes.txt\nn\n/quit\n"[..]),
        Some(harness.broker.gate().subscribe()),
    )
    .expect("a session");
    session.interactive().await.unwrap();

    assert!(
        drain(&mut decisions)
            .iter()
            .any(|decision| matches!(decision.outcome, AuthorizationOutcome::ApprovalRefused)),
        "a refusal must be recorded as a person's decision, not as an error",
    );
    let sent = format!("{:?}", harness.model.requests());
    assert!(!sent.contains("the file's contents"), "{sent}");

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_answer_that_is_not_yes_refuses() {
    // Everything that is not an explicit yes — here, a blank line — is a no.
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(ask_per_file())
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("no luck"))
        .build(root.path())
        .await;

    let mut decisions = harness.kernel.context().subscribe::<AuthorizationDecided>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"read notes.txt\n\n/quit\n"[..]),
        Some(harness.broker.gate().subscribe()),
    )
    .expect("a session");
    session.interactive().await.unwrap();

    assert!(
        drain(&mut decisions)
            .iter()
            .any(|decision| matches!(decision.outcome, AuthorizationOutcome::ApprovalRefused)),
    );
    harness.kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// delegated identity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_calls_are_attributed_to_the_agent_acting_for_the_user() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(allow_reads())
        .user("alice")
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("done"))
        .build(root.path())
        .await;

    let mut decisions = harness.kernel.context().subscribe::<AuthorizationDecided>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    let decisions = drain(&mut decisions);
    assert!(
        !decisions.is_empty(),
        "the policy engine must have been asked"
    );
    for decision in &decisions {
        assert_eq!(decision.principal.as_str(), "assistant");
        assert_eq!(decision.principal_kind, PrincipalKind::Agent);
        assert_eq!(
            decision.on_behalf_of.as_ref().map(|id| id.as_str()),
            Some("alice"),
        );
        assert_ne!(
            decision.principal.as_str(),
            "alice",
            "the frontend must never let a model act under the person's own identity",
        );
    }

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rule_written_for_the_user_does_not_grant_the_agent() {
    // The delegated identity has to be load-bearing, not decorative: a permission the
    // person holds must not be exercisable by the thing acting for them unless a rule says
    // so. `PrincipalMatcher` matching on id is what makes the two distinguishable.
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(json!([
            { "principal": { "id": "alice" }, "action": "filesystem.read",
              "resource": "*", "effect": { "decision": "allow" } },
            { "principal": { "id": "alice" }, "action": "filesystem.read",
              "effect": { "decision": "allow" } }
        ]))
        .user("alice")
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("refused"))
        .build(root.path())
        .await;

    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    assert_eq!(
        drain(&mut invocations)
            .into_iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        vec![InvocationOutcome::Denied],
        "a rule naming the user must not match the agent acting for them",
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_rule_written_for_the_agent_does_grant_it() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(json!([
            { "principal": { "id": "assistant", "kind": "agent" },
              "action": "filesystem.read", "resource": "*",
              "effect": { "decision": "allow" } },
            { "principal": { "id": "assistant", "kind": "agent" },
              "action": "filesystem.read", "effect": { "decision": "allow" } }
        ]))
        .user("alice")
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("done"))
        .build(root.path())
        .await;

    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    assert_eq!(
        drain(&mut invocations)
            .into_iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        vec![InvocationOutcome::Succeeded],
    );

    harness.kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// the frontend can only narrow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_configured_policy_denies_every_tool_call() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .one_shot("read notes.txt")
        .reply(read_call("notes.txt"))
        .reply(Reply::answer("refused"))
        .build(root.path())
        .await;

    assert!(!harness.settings.has_policy());
    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read notes.txt".to_owned()).await.unwrap();

    assert_eq!(
        drain(&mut invocations)
            .into_iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        vec![InvocationOutcome::Denied],
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_write_tool_is_absent_unless_it_was_asked_for() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        // Deliberately permissive: policy alone would allow this.
        .policy(allow_everything())
        .tools(ToolSet::ReadOnly)
        .one_shot("write to notes.txt")
        .reply(Reply::call(
            "c1",
            "filesystem.write",
            json!({ "path": "notes.txt", "contents": "overwritten" }),
        ))
        .reply(Reply::answer("I cannot write"))
        .build(root.path())
        .await;

    let mut invocations = harness.kernel.context().subscribe::<ToolInvoked>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("write".to_owned()).await.unwrap();

    assert!(
        !harness
            .model
            .offered(0)
            .contains(&"filesystem.write".to_owned()),
        "an unregistered tool must not even be offered: {:?}",
        harness.model.offered(0),
    );
    assert!(
        drain(&mut invocations).is_empty(),
        "a tool the agent does not have must not reach the registry at all",
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "the file's contents",
        "nothing was written",
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn asking_for_no_tools_registers_none() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .policy(allow_everything())
        .tools(ToolSet::None)
        .one_shot("read notes.txt")
        .reply(Reply::answer("nothing to use"))
        .build(root.path())
        .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("hello".to_owned()).await.unwrap();

    assert!(harness.model.offered(0).is_empty());
    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_permissive_policy_cannot_reach_outside_the_configured_root() {
    // Confinement is the tool's own invariant, not policy's. This asserts the frontend
    // actually hands the root to the tools, which is the only part of it the CLI owns.
    let outside = tempfile::tempdir().expect("a directory outside the root");
    std::fs::write(outside.path().join("secret.txt"), "not for the agent").expect("a file");
    let root = root_with_secret();

    let escape = outside.path().join("secret.txt");
    let harness = HarnessBuilder::new()
        .policy(allow_everything())
        .one_shot("read the other file")
        .reply(read_call(escape.to_str().expect("utf-8")))
        .reply(Reply::answer("could not"))
        .build(root.path())
        .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("read".to_owned()).await.unwrap();

    let sent = format!("{:?}", harness.model.requests());
    assert!(!sent.contains("not for the agent"), "{sent}");

    harness.kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_closes_the_broker_so_nothing_stays_parked() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("hello"))
        .build(root.path())
        .await;

    assert!(!harness.broker.is_closed());
    harness.kernel.shutdown().await.unwrap();
    assert!(
        harness.broker.is_closed(),
        "a closed broker refuses whatever was still waiting",
    );
}

#[tokio::test]
async fn an_interactive_session_holds_a_gate_only_while_it_lasts() {
    let root = root_with_secret();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("hello"))
        .build(root.path())
        .await;

    assert_eq!(harness.broker.gate_count(), 0, "nothing is listening yet");
    {
        let session = Session::new(
            &harness.kernel.context(),
            &harness.settings,
            Console::new(&b"/quit\n"[..]),
            Some(harness.broker.gate().subscribe()),
        )
        .expect("a session");
        assert_eq!(harness.broker.gate_count(), 1, "the session can be asked");
        drop(session);
    }
    assert_eq!(
        harness.broker.gate_count(),
        0,
        "once the session is gone, approvals must go back to being refused",
    );

    harness.kernel.shutdown().await.unwrap();
}
