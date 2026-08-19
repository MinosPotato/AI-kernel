//! How a conversation behaves over more than one turn.
//!
//! The frontend owns exactly two pieces of conversational state — which session is current,
//! and when to stop — and everything else about a conversation belongs to the
//! [`ContextStore`]. These tests check the two it owns, and check that it did not
//! accidentally acquire a third.

mod support;

use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_cli::console::Console;
use aik_cli::session::{Outcome, Session};
use aik_core::ErrorKind;
use support::{Harness, HarnessBuilder, Reply};

async fn harness(replies: Vec<Reply>, root: &std::path::Path) -> Harness {
    let mut builder = HarnessBuilder::new();
    for reply in replies {
        builder = builder.reply(reply);
    }
    builder.build(root).await
}

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// The transcript of `session`, as the agent principal can see it.
async fn transcript(harness: &Harness, session: SessionId) -> Vec<String> {
    let store = harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a context store");
    let cx = ExecutionContext::new().with_principal(harness.settings.principal());

    store
        .window(&session, &ContextBudget::tokens(1_000_000), &cx)
        .await
        .expect("a window")
        .messages
        .into_iter()
        .flat_map(|message| {
            message
                .content
                .into_iter()
                .filter_map(move |part| match part {
                    ContentPart::Text { text } => Some(format!("{:?}: {text}", message.role)),
                    _ => None,
                })
        })
        .collect()
}

#[tokio::test]
async fn every_turn_continues_the_same_conversation() {
    let root = root();
    let harness = harness(
        vec![
            Reply::answer("first answer"),
            Reply::answer("second answer"),
        ],
        root.path(),
    )
    .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"first question\nsecond question\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let id = session.id();
    assert_eq!(session.interactive().await.unwrap(), Outcome::Quit);

    let transcript = transcript(&harness, id).await;
    assert_eq!(
        transcript,
        vec![
            "User: first question",
            "Assistant: first answer",
            "User: second question",
            "Assistant: second answer",
        ],
        "both turns belong to one transcript, in order",
    );

    // And the model saw the history rather than only the latest line: continuity is the
    // store's, not something the frontend reassembles.
    let second = &harness.model.requests()[1];
    assert!(second.messages.len() > 1, "{:?}", second.messages);

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn slash_new_starts_a_conversation_that_cannot_see_the_last_one() {
    let root = root();
    let harness = harness(
        vec![
            Reply::answer("first answer"),
            Reply::answer("second answer"),
        ],
        root.path(),
    )
    .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"first question\n/new\nsecond question\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let first = session.id();
    session.interactive().await.unwrap();
    let second = session.id();

    assert_ne!(first, second);
    assert_eq!(
        transcript(&harness, second).await,
        vec!["User: second question", "Assistant: second answer"],
        "a new conversation starts empty",
    );
    assert_eq!(
        transcript(&harness, first).await.len(),
        2,
        "and the old one is still there, untouched",
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_transcript_belongs_to_the_agent_principal_not_the_user() {
    // Session ownership is by principal, and the frontend runs as the agent. A frontend
    // that quietly used the person's identity for the store and the agent's for tools would
    // work perfectly and be wrong; this is what tells the two apart.
    let root = root();
    let harness = harness(vec![Reply::answer("hello")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    let id = session.id();
    session.one_shot("hi".to_owned()).await.unwrap();

    let store = harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .unwrap();

    let as_agent = ExecutionContext::new().with_principal(harness.settings.principal());
    assert!(
        store.stats(&id, &as_agent).await.unwrap().is_some(),
        "the agent owns what it wrote",
    );

    let as_user =
        ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User));
    let error = store
        .stats(&id, &as_user)
        .await
        .expect_err("a different principal must not reach it");
    assert_eq!(error.kind(), ErrorKind::Permission);

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_turn_is_reported_and_the_session_carries_on() {
    let root = root();
    let harness = harness(
        vec![
            Reply::fail("the model server went away"),
            Reply::answer("back"),
        ],
        root.path(),
    )
    .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"first\nsecond\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let id = session.id();
    assert_eq!(session.interactive().await.unwrap(), Outcome::Quit);

    assert_eq!(
        transcript(&harness, id).await,
        vec!["User: first", "User: second", "Assistant: back"],
        "the failed turn left its input recorded and no answer, and the next turn worked",
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_one_shot_run_that_fails_reports_the_error_to_its_caller() {
    // Unlike an interactive session, a one-shot run has no next prompt to carry on to: its
    // caller is a script or a process exit code, and it needs to be able to tell a failed
    // run apart from a successful one rather than always seeing `Ok(())`.
    let root = root();
    let harness = harness(vec![Reply::fail("the model server went away")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    let error = session
        .one_shot("hello".to_owned())
        .await
        .expect_err("the model failure must reach the caller");
    assert!(
        error.to_string().contains("the model server went away"),
        "{error}"
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn input_running_out_ends_the_session() {
    let root = root();
    let harness = harness(vec![Reply::answer("only answer")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"a question\n"[..]),
        None,
    )
    .expect("a session");
    assert_eq!(session.interactive().await.unwrap(), Outcome::Quit);
    assert_eq!(harness.model.requests().len(), 1);

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn blank_lines_and_unknown_commands_do_not_reach_the_model() {
    let root = root();
    let harness = harness(vec![Reply::answer("answer")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"\n   \n/nonsense\n/help\nreal question\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    session.interactive().await.unwrap();

    assert_eq!(
        harness.model.requests().len(),
        1,
        "only the one real question was a turn",
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_one_shot_run_is_exactly_one_turn() {
    let root = root();
    let harness = harness(vec![Reply::answer("done")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        // Input that would drive several turns if it were ever read.
        Console::new(&b"another\nand another\n"[..]),
        None,
    )
    .expect("a session");
    session
        .one_shot("the only question".to_owned())
        .await
        .unwrap();

    assert_eq!(harness.model.requests().len(), 1);
    assert_eq!(
        transcript(&harness, session.id()).await,
        vec!["User: the only question", "Assistant: done"],
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_agents_answer_is_recorded_as_the_assistants_rather_than_the_users() {
    let root = root();
    let harness = harness(vec![Reply::answer("mine")], root.path()).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("yours".to_owned()).await.unwrap();

    let store = harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .unwrap();
    let cx = ExecutionContext::new().with_principal(harness.settings.principal());
    let window = store
        .window(&session.id(), &ContextBudget::tokens(1_000_000), &cx)
        .await
        .unwrap();

    assert_eq!(window.messages[0].role, Role::User);
    assert_eq!(window.messages[1].role, Role::Assistant);

    harness.kernel.shutdown().await.unwrap();
}
