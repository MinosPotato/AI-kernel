//! Who may talk to a host, and what they have to prove.
//!
//! Every test here starts the shipped host — the real listener, the real file modes, the real
//! handshake — and then tries to get in. The properties asserted are the ones that would be
//! silently lost by a refactor and only noticed by whoever exploited them.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use aik_core::ErrorKind;
use aik_ipc::protocol::{Hello, PROTOCOL_VERSION, RejectReason, Reply, Request, Welcome};
use aik_ipc::{Client, Endpoint, Token, frame};
use support::{Answers, HostBuilder, Turn, permissive};
use tokio::net::UnixStream;

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

// ---------------------------------------------------------------------------
// the filesystem, which is the check a peer cannot influence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_socket_and_its_token_are_private_to_this_account() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    for (what, path) in [
        ("the socket directory", host.endpoint.directory()),
        ("the socket", host.endpoint.socket()),
        ("the token file", host.endpoint.token()),
    ] {
        let mode = std::fs::metadata(path)
            .unwrap_or_else(|error| panic!("{what} is missing: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "{what} is mode {mode:04o}; nothing may be reachable by another account",
        );
    }

    host.shut_down().await;
}

#[tokio::test]
async fn a_client_refuses_a_socket_it_cannot_vouch_for() {
    // A socket another account could have created is a socket another account could be
    // listening on, and a client that connected to it would hand over its token and then its
    // prompts. The check is the client's own, before the token file is even read.
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    std::fs::set_permissions(
        host.endpoint.socket(),
        std::fs::Permissions::from_mode(0o666),
    )
    .expect("loosened");

    let error = Client::connect(&host.endpoint, "test", false)
        .await
        .expect_err("a socket anyone can reach must not be handed a credential");
    assert_eq!(error.kind(), ErrorKind::Permission);

    std::fs::set_permissions(
        host.endpoint.socket(),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("restored");
    host.shut_down().await;
}

#[tokio::test]
async fn a_client_refuses_a_token_file_others_can_read() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    std::fs::set_permissions(
        host.endpoint.token(),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("loosened");

    let error = Client::connect(&host.endpoint, "test", false)
        .await
        .expect_err("a credential anyone can read is not a credential");
    assert_eq!(error.kind(), ErrorKind::Permission);

    std::fs::set_permissions(
        host.endpoint.token(),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("restored");
    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// the token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_client_with_the_hosts_token_is_accepted() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let (mut client, connected) = host.connect(false).await.expect("accepted");
    assert!(connected.host.starts_with("aikd "), "{connected:?}");
    assert_eq!(
        client.answered(Request::Ping).await.expect("answered"),
        Reply::Pong
    );

    host.shut_down().await;
}

#[tokio::test]
async fn a_client_without_the_hosts_token_is_refused() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let wrong = Token::parse(&"0".repeat(64)).expect("a token shape");
    let stream = UnixStream::connect(host.endpoint.socket())
        .await
        .expect("connected");
    let error = Client::handshake(stream, &wrong, "test", false)
        .await
        .expect_err("a wrong token must not get in");

    assert_eq!(error.kind(), ErrorKind::Permission);
    // The refusal says nothing a peer that has not authenticated could use: not which
    // versions this host speaks, not what the token should have looked like.
    let message = error.to_string();
    assert!(!message.contains("protocol 1"), "{message}");

    host.shut_down().await;
}

#[tokio::test]
async fn a_malformed_handshake_is_refused_exactly_as_a_wrong_token_is() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    for probe in [
        serde_json::json!({ "hello": true }),
        serde_json::json!({ "protocol": PROTOCOL_VERSION }),
        serde_json::json!("a bare string"),
        serde_json::json!(null),
    ] {
        let mut stream = UnixStream::connect(host.endpoint.socket())
            .await
            .expect("connected");
        frame::write(&mut stream, &probe).await.expect("written");

        let welcome = frame::read::<_, Welcome>(&mut stream)
            .await
            .expect("the host answers")
            .expect("with something");
        match welcome {
            Welcome::Rejected { reason, .. } => assert_eq!(
                reason,
                RejectReason::Unauthenticated,
                "{probe} must be indistinguishable from a wrong token",
            ),
            Welcome::Accepted { .. } => panic!("{probe} must not be accepted"),
        }
    }

    host.shut_down().await;
}

#[tokio::test]
async fn a_version_mismatch_is_only_reported_to_a_peer_that_authenticated() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let token = Token::read_from(host.endpoint.token()).expect("the token");

    let mut stream = UnixStream::connect(host.endpoint.socket())
        .await
        .expect("connected");
    frame::write(
        &mut stream,
        &Hello {
            protocol: PROTOCOL_VERSION + 1,
            token: token.as_str().to_owned(),
            client: "test".to_owned(),
            interactive: false,
        },
    )
    .await
    .expect("written");

    match frame::read::<_, Welcome>(&mut stream)
        .await
        .expect("the host answers")
        .expect("with something")
    {
        Welcome::Rejected { reason, message } => {
            assert_eq!(reason, RejectReason::UnsupportedProtocol);
            assert!(message.contains("protocol"), "{message}");
        }
        Welcome::Accepted { .. } => panic!("a host must not speak a version it does not"),
    }

    host.shut_down().await;
}

#[tokio::test]
async fn a_token_does_not_survive_the_host_that_issued_it() {
    // The credential is per-instance, so a client holding a stale one — a process that was
    // connected across a restart, a script that cached it — is refused rather than admitted
    // to a host it never authenticated against.
    let root = root();
    let socket = root.path().join("run").join("aikd.sock");

    let first = HostBuilder::new()
        .ephemeral()
        .socket(&socket)
        .start(root.path())
        .await;
    let stale = Token::read_from(first.endpoint.token()).expect("the token");
    first.shut_down().await;

    let second = HostBuilder::new()
        .ephemeral()
        .socket(&socket)
        .start(root.path())
        .await;
    let fresh = Token::read_from(second.endpoint.token()).expect("the token");
    assert_ne!(
        stale.as_str(),
        fresh.as_str(),
        "a restart must issue a new credential",
    );

    let stream = UnixStream::connect(second.endpoint.socket())
        .await
        .expect("connected");
    let error = Client::handshake(stream, &stale, "test", false)
        .await
        .expect_err("a credential from a host that is gone must not open this one");
    assert_eq!(error.kind(), ErrorKind::Permission);

    second.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_never_finishes_its_handshake_gives_its_slot_back() {
    // The connection limit counts connections that are still handshaking, deliberately: a
    // client in a restart loop must not be able to accumulate half-open connections until the
    // host runs out of file descriptors, which would take the schedule down with it.
    //
    // The cost is that silent peers can fill the limit, and the handshake timeout is what
    // bounds that. Both halves are asserted here, because a change that dropped either one
    // would leave a host that is either unbounded or permanently wedgeable.
    let root = root();
    let host = HostBuilder::new()
        .ephemeral()
        .max_clients(2)
        .start(root.path())
        .await;

    let silent = [
        UnixStream::connect(host.endpoint.socket())
            .await
            .expect("connected"),
        UnixStream::connect(host.endpoint.socket())
            .await
            .expect("connected"),
    ];

    // The host has to have accepted both before the limit is observable; a poll rather than a
    // sleep, because what is being waited for is the accept loop noticing.
    let refused = loop {
        match host.connect(false).await {
            Err(error) => break error,
            Ok(accepted) => drop(accepted),
        }
    };
    assert!(
        refused.to_string().contains("too_many_connections"),
        "a host at its limit says so rather than closing silently: {refused}",
    );

    // Time is paused and auto-advances while the runtime is idle, so the handshake deadline
    // arrives without the test waiting out ten real seconds.
    drop(silent);
    let mut client = loop {
        if let Ok((client, _)) = host.connect(false).await {
            break client;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        client.answered(Request::Ping).await.expect("answered"),
        Reply::Pong,
        "a host must recover its slots from peers that never said anything",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// what an authenticated client is, and is not, given
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_host_reports_the_principal_the_client_cannot_choose() {
    let root = root();
    let host = HostBuilder::new()
        .ephemeral()
        .policy(permissive())
        .says([Turn::answer("hello")])
        .start(root.path())
        .await;

    let (_client, connected) = host.connect(false).await.expect("accepted");

    assert_eq!(connected.principal, host.settings.runtime.principal());
    assert_eq!(
        connected.principal.kind,
        aik_api::permission::PrincipalKind::Agent,
        "a conversation runs as the agent, never as the user it acts for",
    );
    assert_eq!(
        connected
            .principal
            .on_behalf_of
            .as_ref()
            .map(ToString::to_string),
        Some(host.settings.runtime.user.to_string()),
    );

    host.shut_down().await;
}

#[tokio::test]
async fn a_non_interactive_client_holds_no_approval_gate() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let (_quiet, connected) = host.connect(false).await.expect("accepted");
    assert!(
        !connected.interactive,
        "a client that did not say somebody is present must not be given a gate",
    );

    let (_present, connected) = host.connect(true).await.expect("accepted");
    assert!(connected.interactive);

    host.shut_down().await;
}

#[tokio::test]
async fn a_client_that_holds_no_gate_cannot_answer_an_approval() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let mut client = host.client(false).await;
    let error = client
        .answered(Request::Approve {
            approval: aik_approval::ApprovalId::new(),
        })
        .await
        .expect_err("a connection with no gate must not be able to approve anything");

    assert_eq!(error.kind(), ErrorKind::Permission);
    host.shut_down().await;
}

#[tokio::test]
async fn a_socket_in_a_directory_that_does_not_exist_is_created_privately() {
    let root = root();
    let nested = root.path().join("a").join("b").join("aikd.sock");
    let host = HostBuilder::new()
        .ephemeral()
        .socket(&nested)
        .start(root.path())
        .await;

    let mode = std::fs::metadata(host.endpoint.directory())
        .expect("the directory")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);

    host.shut_down().await;
}

#[tokio::test]
async fn an_endpoint_derives_its_token_from_its_socket() {
    // Two settings would be two things to get wrong, and the interesting way to get them
    // wrong — a token belonging to a different host than the socket — is one a client could
    // not detect.
    let endpoint = Endpoint::at("/run/user/1000/aik/aikd.sock");
    assert_eq!(
        endpoint.token(),
        std::path::Path::new("/run/user/1000/aik/aikd.token"),
    );
}
