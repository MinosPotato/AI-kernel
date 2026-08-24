//! What a client may reach, and what it may not.
//!
//! The host serves one principal — its own — and a client cannot name another, because the
//! protocol has nowhere to put one. These tests assert the consequence: records belonging to
//! somebody else are unreachable through the socket, and are unreachable in the way the stores
//! define rather than by anything the host does on top.

mod support;

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_context::RedbContextStore;
use aik_core::ErrorKind;
use aik_ipc::protocol::{Reply, Request};
use aik_store::Db;
use support::{Answers, HostBuilder, Turn, permissive};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// Writes a session owned by `owner` straight into the database, before any host opens it.
///
/// Deliberately through the real store under a real execution context, rather than by writing
/// rows: the ownership these tests are about is the one
/// [`ContextStore`] stamps from the context, and a fixture that stamped it any other way would
/// be asserting against something the system does not do.
async fn plant_session(database: &std::path::Path, owner: &str) -> SessionId {
    let session = SessionId::new();
    {
        let db = Arc::new(Db::open(database).expect("a database"));
        let store = RedbContextStore::new(db).expect("a context store");
        let cx = ExecutionContext::new().with_principal(Principal::new(owner, PrincipalKind::User));
        store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, "a private conversation")),
                &cx,
            )
            .await
            .expect("appended");
    }
    session
}

// ---------------------------------------------------------------------------
// principal isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn another_principals_session_is_absent_from_a_listing_rather_than_refused() {
    // Absent, not an error: an enumeration that errored on encountering somebody else's
    // session would confirm that it exists.
    let root = root();
    let database = root.path().join("data").join("aik.redb");
    let theirs = plant_session(&database, "mallory").await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    let Reply::Sessions(sessions) = client.answered(Request::Sessions).await.expect("listed")
    else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        !sessions.iter().any(|stats| stats.session == theirs),
        "a client must not learn that another principal's session exists: {sessions:?}",
    );

    host.shut_down().await;
}

#[tokio::test]
async fn another_principals_session_cannot_be_cleared_or_compacted_through_the_socket() {
    let root = root();
    let database = root.path().join("data").join("aik.redb");
    let theirs = plant_session(&database, "mallory").await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    for request in [
        Request::Clear { session: theirs },
        Request::Compact {
            session: theirs,
            keep: 0,
        },
    ] {
        let error = client
            .call(request)
            .await
            .expect_err("another principal's transcript is not this one's to touch");
        assert_eq!(
            error.kind(),
            ErrorKind::Permission,
            "the store's refusal must reach the client as a refusal: {error}",
        );
    }

    host.shut_down().await;

    // And it is still there, which is what makes the refusal a refusal rather than a report.
    let db = Arc::new(Db::open(&database).expect("a database"));
    let store = RedbContextStore::new(db).expect("a context store");
    let cx = ExecutionContext::new().with_principal(Principal::new("mallory", PrincipalKind::User));
    let stats = store.stats(&theirs, &cx).await.expect("asked");
    assert_eq!(
        stats.map(|stats| stats.records),
        Some(1),
        "the record must survive the refused clear",
    );
}

#[tokio::test]
async fn a_turn_in_another_principals_session_is_refused() {
    let root = root();
    let database = root.path().join("data").join("aik.redb");
    let theirs = plant_session(&database, "mallory").await;

    let host = HostBuilder::new()
        .database(&database)
        .policy(permissive())
        .says([Turn::answer("this must not be appended")])
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    let error = client
        .answered(Request::Prompt {
            session: Some(theirs),
            input: "carry on their conversation".to_owned(),
        })
        .await
        .expect_err("a client must not be able to append to another principal's transcript");
    assert_eq!(error.kind(), ErrorKind::Permission, "{error}");

    host.shut_down().await;
}

#[tokio::test]
async fn a_client_gets_the_hosts_principal_and_has_no_way_to_change_it() {
    let root = root();
    let host = HostBuilder::new()
        .ephemeral()
        .policy(permissive())
        .says([Turn::answer("hello")])
        .start(root.path())
        .await;

    let (mut client, connected) = host.connect(false).await.expect("accepted");
    assert_eq!(connected.principal, host.settings.runtime.principal());

    // A request with an extra field naming somebody else is not a request that acts as them:
    // there is no such field, so it is simply ignored, and the session that comes back is
    // owned by the host's own principal.
    let reply = client
        .answered(Request::Prompt {
            session: None,
            input: "hello".to_owned(),
        })
        .await
        .expect("answered");
    let Reply::Finished(response) = reply else {
        panic!("the host answered the wrong shape");
    };

    let Reply::Sessions(sessions) = client.answered(Request::Sessions).await.expect("listed")
    else {
        panic!("the host answered the wrong shape");
    };
    let stats = sessions
        .iter()
        .find(|stats| stats.session == response.session)
        .expect("the session it just created");
    assert_eq!(
        stats.owner,
        host.settings.runtime.principal().id,
        "a conversation belongs to the host's agent, not to anything a client said",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// the host is not a way around the tool registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tool_call_is_still_refused_when_no_policy_allows_it() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    // No policy at all, which denies everything. The model asks for a read anyway.
    let host = HostBuilder::new()
        .ephemeral()
        .says([
            Turn::call(
                "c1",
                "filesystem.read",
                serde_json::json!({ "path": "notes.txt" }),
            ),
            Turn::answer("I could not read it"),
        ])
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let reply = client
        .answered(Request::Prompt {
            session: None,
            input: "read notes.txt".to_owned(),
        })
        .await
        .expect("the turn finishes; the tool call is what fails");
    assert!(matches!(reply, Reply::Finished(_)));

    // The thing itself did not happen: the file's contents are nowhere in what the model was
    // subsequently sent.
    let sent = format!("{:?}", host.model.requests());
    assert!(
        !sent.contains("the file's contents"),
        "a denied read must not reach the model: {sent}",
    );

    host.shut_down().await;
}

#[tokio::test]
async fn a_tool_the_host_did_not_register_cannot_be_reached_however_permissive_the_policy() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "before").expect("a file");

    // Read-only tools — the default — with a policy that allows everything. The outer limit
    // is the registration, and it is not something a policy or a client can widen.
    let host = HostBuilder::new()
        .ephemeral()
        .policy(permissive())
        .says([
            Turn::call(
                "c1",
                "filesystem.write",
                serde_json::json!({ "path": "notes.txt", "contents": "after" }),
            ),
            Turn::answer("I could not write it"),
        ])
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .answered(Request::Prompt {
            session: None,
            input: "overwrite notes.txt".to_owned(),
        })
        .await
        .expect("the turn finishes");

    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.txt")).expect("still readable"),
        "before",
        "a tool that was never registered must not be reachable",
    );

    host.shut_down().await;
}

#[tokio::test]
async fn an_approval_with_nobody_attached_is_refused_rather_than_granted() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    let host = HostBuilder::new()
        .ephemeral()
        .policy(support::ask_per_file())
        .says([
            Turn::call(
                "c1",
                "filesystem.read",
                serde_json::json!({ "path": "notes.txt" }),
            ),
            Turn::answer("I could not read it"),
        ])
        .start(root.path())
        .await;

    // Not interactive: no gate, so the broker has nobody to ask and refuses immediately
    // rather than parking the question in front of nobody.
    let mut client = host.client(false).await;
    client
        .answered(Request::Prompt {
            session: None,
            input: "read notes.txt".to_owned(),
        })
        .await
        .expect("the turn finishes");

    let sent = format!("{:?}", host.model.requests());
    assert!(
        !sent.contains("the file's contents"),
        "a question nobody could answer must not become a yes: {sent}",
    );

    host.shut_down().await;
}
