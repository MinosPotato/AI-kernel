//! Starting, stopping, restarting, and the one file that decides who may do any of it.
//!
//! The host exists because redb hands the database to exactly one process. These tests are
//! about what follows from that: what a second host does, what a restart preserves, what is
//! left on disk afterwards, and what a client is told when the host goes away underneath it.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_core::ErrorKind;
use aik_ipc::protocol::{Reply, Request, Response};
use aik_store::Db;
use support::{Answers, HostBuilder, Turn, await_stopped, permissive};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn answers(count: usize) -> Vec<Turn> {
    (0..count)
        .map(|n| Turn::answer(&format!("answer {n}")))
        .collect()
}

// ---------------------------------------------------------------------------
// exactly one host
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_second_host_on_the_same_socket_refuses_to_start() {
    let root = root();
    let first = HostBuilder::new().ephemeral().start(root.path()).await;

    let settings = HostBuilder::new()
        .ephemeral()
        .socket(first.endpoint.socket())
        .settings(root.path(), None);

    let error = aik_daemon::serve(settings, tokio_util::sync::CancellationToken::new())
        .await
        .expect_err("two hosts must not serve one deployment");
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert!(
        error.to_string().contains("already being served"),
        "{error}"
    );

    // And the first is untouched: still listening, still answering.
    let mut client = first.client(false).await;
    assert_eq!(
        client.answered(Request::Ping).await.expect("answered"),
        Reply::Pong,
    );

    first.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_host_on_the_same_database_refuses_to_start_even_on_another_socket() {
    // The socket is one lock and the database is another. A deployment that moved the socket
    // but kept the database must still fail, because two kernels over one redb file is the
    // thing that cannot happen.
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    let first = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;

    let settings = HostBuilder::new()
        .database(&database)
        .socket(root.path().join("run").join("second.sock"))
        .settings(root.path(), None);

    let error = aik_daemon::serve(settings, tokio_util::sync::CancellationToken::new())
        .await
        .expect_err("two kernels must not share one database");
    // Whatever redb calls it, it is a failure to start and not a second host.
    assert!(!error.to_string().is_empty(), "{error}");
    assert!(
        !root.path().join("run").join("second.sock").exists(),
        "a host that could not start must leave no socket behind",
    );

    first.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_database_is_held_while_the_host_runs_and_released_when_it_stops() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;

    assert!(
        Db::open(&database).is_err(),
        "the whole reason this process exists is that the database is held exclusively",
    );

    host.shut_down().await;

    // Released on the host's *drop*, not merely on its shutdown — which is why the serving
    // task owns the kernel and drops it before returning.
    let reopened = Db::open(&database).expect("the database is released when the host goes");
    drop(reopened);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_cannot_start_leaves_nothing_behind() {
    let root = root();
    // A policy document that is not a policy: the engine reads it during assembly, so this
    // fails before anything is bound.
    let policy = root.path().join("broken.json");
    std::fs::write(&policy, r#"{ "rules": [ { "nonsense": true } ] }"#).expect("written");

    let mut options = aik_daemon::args::Options {
        ephemeral: true,
        policy: Some(policy),
        root: Some(root.path().to_path_buf()),
        socket: Some(root.path().join("run").join("aikd.sock")),
        model: Some(support::SCRIPTED.to_owned()),
        ..aik_daemon::args::Options::default()
    };
    options.max_connections = Some(4);

    let settings = aik_daemon::settings::DaemonSettings::resolve_from(
        &options,
        Vec::<(String, String)>::new(),
    )
    .expect("resolved");

    let error = aik_daemon::serve(settings, tokio_util::sync::CancellationToken::new())
        .await
        .expect_err("a malformed policy must stop the host coming up");
    assert!(!error.to_string().is_empty(), "{error}");
    assert!(
        !root.path().join("run").join("aikd.sock").exists(),
        "a failed start must not leave a socket a client could connect to",
    );
}

// ---------------------------------------------------------------------------
// stopping
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stopping_removes_the_socket_and_the_token() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let socket = host.endpoint.socket().to_path_buf();
    let token = host.endpoint.token().to_path_buf();

    host.stop().await.expect("the host stops cleanly");

    assert!(!socket.exists(), "a socket must not outlive its host");
    assert!(
        !token.exists(),
        "a credential must not outlive the host it authenticates",
    );
    await_stopped(&socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_client_is_told_the_host_is_going_rather_than_merely_disconnected() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let mut client = host.client(false).await;

    host.shutdown.cancel();

    let told = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("read") {
                Some(Response::Closing { message }) => return Some(message),
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await
    .expect("the host says goodbye promptly");

    assert!(
        told.is_some_and(|message| message.contains("shutting down")),
        "a client should be able to say why it lost the host",
    );

    host.stop().await.expect("stopped cleanly");
}

#[tokio::test(flavor = "multi_thread")]
async fn work_in_flight_is_answered_before_the_host_returns() {
    // Cancelled, and *said* to be cancelled. A host that dropped its connections on the way
    // out would leave a client holding a call it never hears about again, which is
    // indistinguishable from the host having hung.
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        .slow(Duration::from_secs(120))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let id = client
        .send(Request::Prompt {
            session: None,
            input: "take forever".to_owned(),
        })
        .await
        .expect("sent");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while host.model.completions() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never reached the model",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Stopped *first*, and only then read. The ordering is the assertion: everything this
    // client will ever receive has already been written by the time the host returns, so a
    // host that dropped its connections on the way out has nothing here to find.
    host.stop().await.expect("stopped cleanly");

    let answered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("read") {
                Some(Response::Done { id: answered, .. })
                | Some(Response::Failed { id: answered, .. })
                    if answered == id =>
                {
                    return true;
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .expect("the frames are already buffered, so this cannot block");

    assert!(
        answered,
        "the call in flight must be answered before the host returns, not dropped with the \
         connection",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_work_in_flight_rather_than_waiting_it_out() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        // Far longer than any shutdown should wait for.
        .slow(Duration::from_secs(120))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .send(Request::Prompt {
            session: None,
            input: "take forever".to_owned(),
        })
        .await
        .expect("sent");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while host.model.completions() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never reached the model",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let started = std::time::Instant::now();
    host.shut_down().await;
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "shutdown waited for work it should have cancelled: {:?}",
        started.elapsed(),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_is_shutting_down_accepts_nobody_new() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let endpoint = host.endpoint.clone();

    host.shutdown.cancel();
    host.stop().await.expect("stopped cleanly");

    let error = aik_ipc::Client::connect(&endpoint, "test", false)
        .await
        .expect_err("there is nothing left to connect to");
    assert!(!error.to_string().is_empty(), "{error}");
}

// ---------------------------------------------------------------------------
// restarting
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_survives_a_restart() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");
    let socket = root.path().join("run").join("aikd.sock");

    let session = {
        let host = HostBuilder::new()
            .database(&database)
            .socket(&socket)
            .policy(permissive())
            .says(answers(2))
            .start(root.path())
            .await;

        let mut client = host.client(false).await;
        let Reply::Finished(response) = client
            .answered(Request::Prompt {
                session: None,
                input: "remember this".to_owned(),
            })
            .await
            .expect("answered")
        else {
            panic!("the host answered the wrong shape");
        };

        host.shut_down().await;
        response.session
    };

    let host = HostBuilder::new()
        .database(&database)
        .socket(&socket)
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let Reply::Sessions(sessions) = client.answered(Request::Sessions).await.expect("listed")
    else {
        panic!("the host answered the wrong shape");
    };
    let stats = sessions
        .iter()
        .find(|stats| stats.session == session)
        .expect("the conversation from before the restart");
    assert!(
        stats.records >= 2,
        "the transcript must survive, not merely the session id: {stats:?}",
    );
    assert_eq!(
        stats.owner,
        host.settings.runtime.principal().id,
        "ownership must survive the restart too",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_host_remembers_nothing_across_a_restart() {
    let root = root();
    let socket = root.path().join("run").join("aikd.sock");

    {
        let host = HostBuilder::new()
            .ephemeral()
            .socket(&socket)
            .policy(permissive())
            .says(answers(2))
            .start(root.path())
            .await;
        let mut client = host.client(false).await;
        client
            .answered(Request::Prompt {
                session: None,
                input: "forget this".to_owned(),
            })
            .await
            .expect("answered");
        host.shut_down().await;
    }

    let host = HostBuilder::new()
        .ephemeral()
        .socket(&socket)
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let Reply::Sessions(sessions) = client.answered(Request::Sessions).await.expect("listed")
    else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        sessions.is_empty(),
        "an ephemeral host promised nothing would reach the disk: {sessions:?}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_replaces_a_socket_a_crashed_host_left_behind() {
    let root = root();
    let socket = root.path().join("run").join("aikd.sock");
    std::fs::create_dir_all(socket.parent().expect("a parent")).expect("created");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            socket.parent().expect("a parent"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("tightened");
    }

    // A crash: the socket file is still there, and nothing is listening on it. `std`'s
    // listener does not unlink on drop, which is exactly the leftover being simulated.
    let dead = std::os::unix::net::UnixListener::bind(&socket).expect("bound");
    drop(dead);
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("tightened");
    }

    let host = HostBuilder::new()
        .ephemeral()
        .socket(&socket)
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    assert_eq!(
        client.answered(Request::Ping).await.expect("answered"),
        Reply::Pong,
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// the audit trail outlives the process that wrote it
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn what_the_host_was_allowed_to_do_is_still_there_after_it_stops() {
    let data = root();
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");
    let database = data.path().join("aik.redb");

    {
        let host = HostBuilder::new()
            .database(&database)
            .policy(permissive())
            .says([
                Turn::call(
                    "c1",
                    "filesystem.read",
                    serde_json::json!({ "path": "notes.txt" }),
                ),
                Turn::answer("I read it"),
            ])
            .start(root.path())
            .await;

        let mut client = host.client(false).await;
        client
            .answered(Request::Prompt {
                session: None,
                input: "read notes.txt".to_owned(),
            })
            .await
            .expect("answered");
        host.shut_down().await;
    }

    // Opened directly, as `aik audit` does when no host is running.
    let db = Arc::new(Db::open(&database).expect("the database is released"));
    let store = aik_audit::RedbAuditStore::new(db).expect("an audit store");
    let issued = aik_api::audit::AuditStore::last_sequence(&store)
        .await
        .expect("asked");
    assert!(
        issued > 0,
        "every authorization decision and tool call is recorded durably",
    );
}
