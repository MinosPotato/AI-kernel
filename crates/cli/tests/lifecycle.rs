//! Managing durable sessions from the terminal.
//!
//! Every test here starts [`aik_cli::wiring::builder`] — the production assembly — over a
//! temporary database, resumes or manages a session through the real
//! [`ContextStore`](aik_api::context::ContextStore), and asserts on what survived and what
//! was refused. The only stub is the model, for the reason [`support`] gives.
//!
//! # Why none of this can touch a real database
//!
//! The same three independent guards `durable.rs` relies on. The path is always passed
//! explicitly as `--db`; the environment handed to `Settings::resolve_from` is empty, so the
//! XDG default has nothing to resolve *from* and refuses rather than guessing; and redb takes
//! an exclusive lock, so a test that somehow reached a live database would fail to open it
//! rather than write to it.
//!
//! # What is actually being asserted
//!
//! Not that the commands work — that is the store's suite. What is asserted here is that the
//! frontend adds no judgement of its own on the way through: it does not authorize, does not
//! filter, does not substitute a new session for a missing one, and does not learn anything
//! about a session it may not touch.

mod support;

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_cli::console::Console;
use aik_cli::session::{Outcome, Session};
use aik_core::ErrorKind;
use support::{Harness, HarnessBuilder, Reply};

/// A root for the filesystem tools and a separate directory for the database.
struct Workspace {
    root: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("a temporary root"),
            data: tempfile::tempdir().expect("a temporary data directory"),
        }
    }

    fn database(&self) -> std::path::PathBuf {
        self.data.path().join("aik.redb")
    }

    async fn open(&self, builder: HarnessBuilder) -> Harness {
        builder
            .database(self.database())
            .build(self.root.path())
            .await
    }
}

/// The store a run published, which is the same one the agent writes through.
fn store(harness: &Harness) -> Arc<dyn ContextStore> {
    harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("the context store is published")
}

/// The text of a session's transcript, as the run's own principal sees it.
async fn transcript(harness: &Harness, session: SessionId) -> Vec<String> {
    store(harness)
        .window(&session, &ContextBudget::UNLIMITED, &harness.cx())
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

/// Drives an interactive session over scripted input, returning what it printed nothing of —
/// the assertions are on the store.
async fn drive(harness: &Harness, input: &'static str) -> (SessionId, Outcome) {
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(input.as_bytes()),
        None,
    )
    .expect("a session");
    session
        .resume(&harness.settings)
        .await
        .expect("the session resumes");
    let id = session.id();
    let outcome = session.interactive().await.expect("the session runs");
    (id, outcome)
}

#[tokio::test]
async fn a_resumed_session_continues_the_earlier_conversation() {
    let workspace = Workspace::new();

    // First run: an ordinary conversation, whose id is all the second run is given.
    let first = workspace
        .open(HarnessBuilder::new().reply(Reply::answer("the first answer")))
        .await;
    let (session, _) = drive(&first, "the first question\n/quit\n").await;
    assert_eq!(transcript(&first, session).await.len(), 2);
    first.stop().await;

    // Second run: same database, `--session` naming the first run's conversation.
    let second = workspace
        .open(
            HarnessBuilder::new()
                .session(session)
                .reply(Reply::answer("the second answer")),
        )
        .await;
    let (resumed, _) = drive(&second, "the second question\n/quit\n").await;
    assert_eq!(
        resumed, session,
        "the run adopts the requested id rather than minting one"
    );

    assert_eq!(
        transcript(&second, session).await,
        vec![
            "User: the first question",
            "Assistant: the first answer",
            "User: the second question",
            "Assistant: the second answer",
        ],
        "resuming appends to the transcript rather than starting beside it",
    );

    // And the model was shown the earlier turns, which is the whole point of resuming: the
    // continuity is the store's, not something the frontend reassembled.
    let sent = &second.model.requests()[0];
    assert!(
        sent.messages.len() > 1,
        "a resumed turn carries the history: {:?}",
        sent.messages
    );
    second.stop().await;
}

#[tokio::test]
async fn resuming_a_session_that_does_not_exist_is_an_error_not_a_new_one() {
    let workspace = Workspace::new();
    let absent = SessionId::new();
    let harness = workspace.open(HarnessBuilder::new().session(absent)).await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");

    let error = session
        .resume(&harness.settings)
        .await
        .expect_err("a session that was never written cannot be resumed");
    drop(session);
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    assert!(error.to_string().contains(&absent.to_string()), "{error}");

    // Nothing was written on the way to failing. A frontend that had started a replacement
    // conversation would have left one here, and the person would be talking to it.
    assert!(
        store(&harness)
            .sessions(&harness.cx())
            .await
            .unwrap()
            .is_empty()
    );
    harness.stop().await;
}

#[tokio::test]
async fn resuming_another_principals_session_is_refused_by_the_store() {
    let workspace = Workspace::new();

    // A session written directly, owned by somebody this run is not and does not act for.
    let hers = SessionId::new();
    let setup = workspace.open(HarnessBuilder::new()).await;
    let mallory =
        ExecutionContext::new().with_principal(Principal::new("mallory", PrincipalKind::User));
    store(&setup)
        .append(
            &hers,
            ContextEntry::new(Message::text(Role::User, "mallory's secret")),
            &mallory,
        )
        .await
        .unwrap();
    setup.stop().await;

    let harness = workspace.open(HarnessBuilder::new().session(hers)).await;
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");

    // Refused as `PermissionDenied`, not as "no such session" and not as a fresh session.
    // The kind is what proves the frontend propagated the store's answer rather than
    // reaching its own: nothing in `aik-cli` can produce this error.
    let error = session
        .resume(&harness.settings)
        .await
        .expect_err("another principal's session is not resumable");
    drop(session);
    assert_eq!(error.kind(), ErrorKind::Permission, "{error}");

    // And the refusal changed nothing about the session it refused.
    assert_eq!(
        store(&harness)
            .stats(&hers, &mallory)
            .await
            .unwrap()
            .unwrap()
            .records,
        1
    );
    harness.stop().await;
}

#[tokio::test]
async fn slash_sessions_lists_only_what_the_run_may_act_for() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().reply(Reply::answer("mine")))
        .await;

    // One session this run owns, written the ordinary way.
    let (ours, _) = drive(&harness, "a question\n/quit\n").await;

    // And one it does not, written directly under another principal.
    let theirs = SessionId::new();
    store(&harness)
        .append(
            &theirs,
            ContextEntry::new(Message::text(Role::User, "not for you")),
            &ExecutionContext::new().with_principal(Principal::new("mallory", PrincipalKind::User)),
        )
        .await
        .unwrap();

    // What `/sessions` renders is exactly this, unfiltered by the frontend — which is why
    // asserting on the store's answer is asserting on the command.
    let listed = store(&harness).sessions(&harness.cx()).await.unwrap();
    let ids: Vec<SessionId> = listed.iter().map(|stats| stats.session).collect();
    assert_eq!(ids, vec![ours]);
    assert!(
        !ids.contains(&theirs),
        "another principal's conversation must not appear in a listing"
    );

    // And — the part asserting on the store cannot reach — what the command *itself* shows.
    // A `/sessions` that asked the store the wrong question would satisfy every assertion
    // above and still print Mallory's conversation to Alice's terminal, so the rows the
    // command produces are checked directly.
    let session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"/quit\n"[..]),
        None,
    )
    .expect("a session");
    let shown = session.list_sessions().await.expect("the command runs");
    assert_eq!(
        shown.iter().map(|stats| stats.session).collect::<Vec<_>>(),
        vec![ours],
        "the command shows what this run may act for, and nothing else",
    );
    assert!(
        shown
            .iter()
            .all(|stats| stats.owner == harness.settings.principal().id
                || Some(&stats.owner) == harness.settings.principal().on_behalf_of.as_ref()),
        "every row belongs to the run's own principal or the one it acts for: {shown:?}",
    );

    // The command itself also runs through the dispatcher without error.
    drop(session);
    let (_, outcome) = drive(&harness, "/sessions\n/quit\n").await;
    assert_eq!(outcome, Outcome::Quit);
    harness.stop().await;
}

#[tokio::test]
async fn slash_clear_removes_the_session_from_the_database() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().reply(Reply::answer("an answer")))
        .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"a question\n/clear\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let id = session.id();
    session.interactive().await.unwrap();

    assert!(
        store(&harness)
            .stats(&id, &harness.cx())
            .await
            .unwrap()
            .is_none()
    );
    // The session holds the store, which holds the database. redb's lock is released only
    // when the last handle goes, so a test that reopened while still holding one would be
    // asserting about its own grip rather than about the file.
    drop(session);
    harness.stop().await;

    // Reopened, because "cleared" has to mean the file rather than a handle's memory of it.
    let reopened = workspace.open(HarnessBuilder::new()).await;
    assert!(
        store(&reopened)
            .stats(&id, &reopened.cx())
            .await
            .unwrap()
            .is_none(),
        "a cleared session must not come back with the next process"
    );
    assert!(
        store(&reopened)
            .sessions(&reopened.cx())
            .await
            .unwrap()
            .is_empty()
    );
    reopened.stop().await;
}

#[tokio::test]
async fn slash_compact_makes_a_full_session_appendable_again() {
    let workspace = Workspace::new();
    let harness = workspace.open(HarnessBuilder::new()).await;
    let store = store(&harness);
    let cx = harness.cx();

    // A session filled past what `/compact`'s default keeps, written directly so the test is
    // about the command rather than about how many turns a model takes.
    let session = SessionId::new();
    for index in 0..150 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("turn {index}"))),
                &cx,
            )
            .await
            .unwrap();
    }
    let before = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(before.records, 150);

    // `/compact` acts on the current session, so the run has to be pointed at this one —
    // which is exactly what `--session` does in a real invocation.
    let mut settings = harness.settings.clone();
    settings.session = Some(session);
    let mut driven = Session::new(
        &harness.kernel.context(),
        &settings,
        Console::new(&b"/compact\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    driven.resume(&settings).await.expect("resumed");
    assert_eq!(driven.id(), session);
    driven.interactive().await.unwrap();

    let after = store.stats(&session, &cx).await.unwrap().unwrap();
    assert_eq!(after.records, 100, "the default keeps the newest hundred");
    assert!(after.tokens < before.tokens);

    // Appending still works, which is the state compaction exists to restore.
    store
        .append(
            &session,
            ContextEntry::new(Message::text(Role::User, "after compaction")),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(
        store.stats(&session, &cx).await.unwrap().unwrap().records,
        101
    );
    drop(driven);
    drop(store);
    harness.stop().await;
}

#[tokio::test]
async fn slash_compact_takes_an_explicit_count_and_rejects_a_bad_one() {
    let workspace = Workspace::new();
    let harness = workspace.open(HarnessBuilder::new()).await;
    let store = store(&harness);
    let cx = harness.cx();

    let session = SessionId::new();
    for index in 0..10 {
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("turn {index}"))),
                &cx,
            )
            .await
            .unwrap();
    }

    let mut settings = harness.settings.clone();
    settings.session = Some(session);

    // A malformed count is reported and changes nothing; the valid one that follows still
    // works, so a typo costs a line rather than the session.
    let mut driven = Session::new(
        &harness.kernel.context(),
        &settings,
        Console::new(&b"/compact three\n/compact 2\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    driven.interactive().await.unwrap();

    assert_eq!(
        store.stats(&session, &cx).await.unwrap().unwrap().records,
        2
    );
    drop(driven);
    drop(store);
    harness.stop().await;
}

#[tokio::test]
async fn slash_clear_and_slash_new_stay_different_commands() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().reply(Reply::answer("an answer")))
        .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"a question\n/clear\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let before = session.id();
    session.interactive().await.unwrap();

    // `/clear` destroys the transcript and stays on the same id. Silently starting a new one
    // would make the destructive half unavoidable for anyone who wanted only the other.
    assert_eq!(session.id(), before);
    drop(session);
    harness.stop().await;
}

#[tokio::test]
async fn an_ephemeral_run_is_unaffected_by_the_lifecycle_commands() {
    // Nothing here reaches a disk, and the commands still behave: an ephemeral run has a
    // context store like any other, it simply does not outlive the process.
    let root = tempfile::tempdir().expect("a temporary root");
    let harness = HarnessBuilder::new()
        .ephemeral()
        .reply(Reply::answer("an answer"))
        .build(root.path())
        .await;

    assert_eq!(harness.settings.database(), None);

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"a question\n/sessions\n/compact 0\n/clear\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    let id = session.id();
    assert_eq!(session.interactive().await.unwrap(), Outcome::Quit);

    assert!(
        store(&harness)
            .stats(&id, &harness.cx())
            .await
            .unwrap()
            .is_none()
    );
    drop(session);
    harness.stop().await;
}
