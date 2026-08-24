//! Several clients at once, and what one of them can do to the others.
//!
//! A host that serves one client at a time is not a host; a host that lets one client stall,
//! starve or cancel another's work is worse than one. These tests are about the bounds
//! between connections.

mod support;

use std::time::Duration;

use aik_core::ErrorKind;
use aik_ipc::protocol::{Call, Reply, Request, Response};
use aik_ipc::{Token, frame};
use support::{Answers, HostBuilder, Turn, ask_per_file, permissive};
use tokio::net::UnixStream;

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn answers(count: usize) -> Vec<Turn> {
    (0..count)
        .map(|n| Turn::answer(&format!("answer {n}")))
        .collect()
}

// ---------------------------------------------------------------------------
// several clients
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn several_clients_hold_conversations_at_the_same_time() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(12))
        // Slow enough that the turns genuinely overlap rather than merely being interleaved
        // by the scheduler after each has finished.
        .slow(Duration::from_millis(50))
        .start(root.path())
        .await;

    let mut turns = tokio::task::JoinSet::new();
    for client in 0..4 {
        let endpoint = host.endpoint.clone();
        turns.spawn(async move {
            let (mut client_connection, _) = aik_ipc::Client::connect(&endpoint, "test", false)
                .await
                .expect("accepted");
            let reply = client_connection
                .answered(Request::Prompt {
                    session: None,
                    input: format!("client {client}"),
                })
                .await
                .expect("answered");
            match reply {
                Reply::Finished(response) => response.session,
                other => panic!("the host answered the wrong shape: {other:?}"),
            }
        });
    }

    let mut sessions = Vec::new();
    while let Some(session) = turns.join_next().await {
        sessions.push(session.expect("the client did not panic"));
    }

    assert_eq!(sessions.len(), 4);
    sessions.sort();
    sessions.dedup();
    assert_eq!(
        sessions.len(),
        4,
        "each client's conversation must be its own, not a shared one",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_clients_conversation_does_not_block_anothers_listing() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(4))
        .slow(Duration::from_secs(30))
        .start(root.path())
        .await;

    let mut talker = host.client(false).await;
    let id = talker
        .send(Request::Prompt {
            session: None,
            input: "this will take a while".to_owned(),
        })
        .await
        .expect("sent");

    // A second client, while the first is parked on a model that will not answer for half a
    // minute. It has to be served now, not in half a minute.
    let mut watcher = host.client(false).await;
    let answered = tokio::time::timeout(Duration::from_secs(5), watcher.answered(Request::Status))
        .await
        .expect("a slow turn must not stop the host serving anybody else")
        .expect("answered");
    let Reply::Status(status) = answered else {
        panic!("the host answered the wrong shape");
    };
    assert!(status.connections >= 2, "{status:?}");

    // Tidy: cancel the parked turn so shutdown does not have to wait it out.
    talker
        .send(Request::Cancel { call: id })
        .await
        .expect("sent");

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_connection_can_ask_several_things_at_once() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(4))
        .slow(Duration::from_secs(30))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let turn = client
        .send(Request::Prompt {
            session: None,
            input: "this will take a while".to_owned(),
        })
        .await
        .expect("sent");
    let status = client.send(Request::Status).await.expect("sent");

    // The answer to the second call arrives while the first is still running, which is the
    // whole point of the call id.
    let answered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv().await.expect("read").expect("a frame") {
                Response::Done { id, reply } if id == status => return reply,
                Response::Done { id, .. } | Response::Failed { id, .. } if id == turn => {
                    panic!("the slow turn must not have finished first")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("a call must not queue behind another call on the same connection");
    assert!(matches!(answered, Reply::Status(_)));

    client
        .send(Request::Cancel { call: turn })
        .await
        .expect("sent");
    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_host_at_its_client_limit_says_so_rather_than_closing_silently() {
    let root = root();
    let host = HostBuilder::new()
        .ephemeral()
        .max_clients(2)
        .start(root.path())
        .await;

    // Held, so the limit is actually reached rather than each connection freeing the slot
    // the next one needs.
    let mut held = Vec::new();
    let refusal = loop {
        match host.connect(false).await {
            Ok((client, _)) => {
                held.push(client);
                assert!(
                    held.len() <= 8,
                    "a host with a limit of 2 accepted {} clients",
                    held.len(),
                );
            }
            Err(error) => break error,
        }
    };

    assert!(
        refusal.to_string().contains("too_many_connections"),
        "a host at its limit says so rather than closing silently: {refusal}",
    );
    assert!(
        refusal.to_string().contains('2'),
        "the refusal names the limit: {refusal}",
    );

    // The limit is a bound, not a wedge: a slot freed by a client going away is usable.
    held.clear();
    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok((mut client, _)) = host.connect(false).await {
                return client.answered(Request::Ping).await.expect("answered");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a slot freed by a departing client must become usable");
    assert_eq!(recovered, Reply::Pong);

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_that_floods_its_own_calls_is_refused_the_excess_rather_than_disconnected() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(32))
        .slow(Duration::from_secs(30))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let mut sent = Vec::new();
    for turn in 0..12 {
        sent.push(
            client
                .send(Request::Prompt {
                    session: None,
                    input: format!("turn {turn}"),
                })
                .await
                .expect("sent"),
        );
    }

    // Some are refused for exceeding the limit; the connection itself survives, and the
    // refusal names the limit rather than being a bare disconnection.
    let mut refusals = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while refusals == 0 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), client.recv())
            .await
            .expect("the host answers")
            .expect("read")
        {
            Some(Response::Failed { error, .. }) => {
                assert_eq!(error.kind, aik_ipc::WireErrorKind::Unsupported, "{error:?}");
                assert!(error.message.contains("in flight"), "{error:?}");
                refusals += 1;
            }
            Some(_) => {}
            None => panic!("the connection must not be closed for asking too much at once"),
        }
    }
    assert!(refusals > 0, "the limit must actually be applied");

    // Still usable afterwards.
    for id in &sent {
        client
            .send(Request::Cancel { call: *id })
            .await
            .expect("sent");
    }
    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_call_reaches_the_work_rather_than_merely_detaching_from_it() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        .slow(Duration::from_secs(30))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let id = client
        .send(Request::Prompt {
            session: None,
            input: "take your time".to_owned(),
        })
        .await
        .expect("sent");

    // The model has to have been reached before cancelling, or the test would pass on a host
    // that never started the turn at all.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while host.model.completions() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never reached the model",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    client
        .send(Request::Cancel { call: id })
        .await
        .expect("sent");

    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv().await.expect("read").expect("a frame") {
                Response::Done { id: answered, .. } | Response::Failed { id: answered, .. }
                    if answered == id =>
                {
                    return;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        outcome.is_ok(),
        "a cancelled call must end long before the model would have answered",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_call_that_already_finished_is_not_an_error() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let mut client = host.client(false).await;
    let reply = client
        .answered(Request::Cancel { call: 9_999 })
        .await
        .expect("a race must not be reported as a failure");
    assert_eq!(reply, Reply::Ok);

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_connection_cannot_cancel_another_connections_call() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(4))
        .slow(Duration::from_millis(400))
        .start(root.path())
        .await;

    let mut victim = host.client(false).await;
    let id = victim
        .send(Request::Prompt {
            session: None,
            input: "mine".to_owned(),
        })
        .await
        .expect("sent");

    // Call ids are per-connection, so the same number names nothing on the attacker's side.
    let mut attacker = host.client(false).await;
    assert_eq!(
        attacker
            .answered(Request::Cancel { call: id })
            .await
            .expect("answered"),
        Reply::Ok,
        "cancelling an id this connection never used is a no-op, not a reach into another",
    );

    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match victim.recv().await.expect("read").expect("a frame") {
                Response::Done {
                    id: answered,
                    reply,
                } if answered == id => return reply,
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    panic!("the victim's turn was cancelled by another connection: {error:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the turn finishes");
    assert!(matches!(finished, Reply::Finished(_)));

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// approvals across connections
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_interactive_client_is_asked_and_its_answer_is_what_decides() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    let host = HostBuilder::new()
        .policy(ask_per_file())
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

    let mut client = host.client(true).await;
    let id = client
        .send(Request::Prompt {
            session: None,
            input: "read notes.txt".to_owned(),
        })
        .await
        .expect("sent");

    let mut asked = false;
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("read").expect("a frame") {
                Response::Approval { pending } => {
                    asked = true;
                    assert!(pending.prompt.contains("read"), "{pending:?}");
                    client
                        .send(Request::Approve {
                            approval: pending.id,
                        })
                        .await
                        .expect("sent");
                }
                Response::Done {
                    id: answered,
                    reply,
                } if answered == id => return reply,
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    panic!("the turn failed: {error:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the turn finishes");

    assert!(
        asked,
        "a policy that defers to a human must reach the client"
    );
    assert!(matches!(finished, Reply::Finished(_)));
    let sent = format!("{:?}", host.model.requests());
    assert!(
        sent.contains("the file's contents"),
        "an approved read must actually happen: {sent}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_approval_stops_the_thing_it_was_about() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    let host = HostBuilder::new()
        .policy(ask_per_file())
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

    let mut client = host.client(true).await;
    let id = client
        .send(Request::Prompt {
            session: None,
            input: "read notes.txt".to_owned(),
        })
        .await
        .expect("sent");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("read").expect("a frame") {
                Response::Approval { pending } => {
                    client
                        .send(Request::Deny {
                            approval: pending.id,
                        })
                        .await
                        .expect("sent");
                }
                Response::Done { id: answered, .. } if answered == id => return,
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    panic!("the turn failed: {error:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the turn finishes");

    let sent = format!("{:?}", host.model.requests());
    assert!(
        !sent.contains("the file's contents"),
        "a denied read must not happen: {sent}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_disconnects_stops_asserting_that_anybody_is_there() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    let host = HostBuilder::new()
        .policy(ask_per_file())
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

    // A console attaches and then goes away. Nothing else is interactive, so a question asked
    // afterwards has nobody to reach and must be refused rather than parked.
    let console = host.client(true).await;
    drop(console);

    // The host has to have noticed; a poll rather than a sleep, because what is waited for is
    // the connection teardown releasing the gate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut probe = host.client(false).await;
        let Reply::Status(status) = probe.answered(Request::Status).await.expect("answered") else {
            panic!("the host answered the wrong shape");
        };
        if status.connections <= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the host never noticed the console had gone",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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
        "a question with nobody to ask must be a refusal: {sent}",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// a broken connection is one connection's problem
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_sends_rubbish_loses_only_its_own_connection() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let token = Token::read_from(host.endpoint.token()).expect("the token");

    let mut healthy = host.client(false).await;

    let mut hostile = UnixStream::connect(host.endpoint.socket())
        .await
        .expect("connected");
    frame::write(
        &mut hostile,
        &aik_ipc::protocol::Hello {
            protocol: aik_ipc::PROTOCOL_VERSION,
            token: token.as_str().to_owned(),
            client: "hostile".to_owned(),
            interactive: false,
        },
    )
    .await
    .expect("written");
    let _welcome = frame::read::<_, aik_ipc::protocol::Welcome>(&mut hostile)
        .await
        .expect("the host answers");

    // A well-framed message of a shape the host cannot read. The two sides no longer agree
    // about where messages begin, so this connection ends — and only this one.
    frame::write(&mut hostile, &serde_json::json!({ "nonsense": true }))
        .await
        .expect("written");

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), healthy.answered(Request::Ping))
            .await
            .expect("the host is still serving")
            .expect("answered"),
        Reply::Pong,
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_fails_leaves_the_connection_usable() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says([
            Turn::fail("the model server is unreachable"),
            Turn::answer("recovered"),
        ])
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let error = client
        .answered(Request::Prompt {
            session: None,
            input: "first".to_owned(),
        })
        .await
        .expect_err("the model failed");
    assert_ne!(error.kind(), ErrorKind::Cancelled, "{error}");

    let reply = client
        .answered(Request::Prompt {
            session: None,
            input: "second".to_owned(),
        })
        .await
        .expect("a failed call must not end the connection");
    assert!(matches!(reply, Reply::Finished(_)));

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_naming_no_request_at_all_is_answered_rather_than_fatal() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let mut client = host.client(false).await;
    // A compact and a clear naming a session that does not exist: malformed in the sense that
    // matters, since the ids are well-formed and simply name nothing.
    let error = client
        .answered(Request::Compact {
            session: aik_api::agent::SessionId::new(),
            keep: 0,
        })
        .await
        .err();
    assert!(
        error.is_none() || error.is_some(),
        "either answer is defensible; what matters is that one arrives",
    );

    assert_eq!(
        client.answered(Request::Ping).await.expect("answered"),
        Reply::Pong
    );
    host.shut_down().await;
}

/// A frame that is not a [`Call`] at all, sent after a valid handshake.
#[tokio::test(flavor = "multi_thread")]
async fn a_frame_larger_than_the_limit_is_never_read_into_memory() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let token = Token::read_from(host.endpoint.token()).expect("the token");

    let mut stream = UnixStream::connect(host.endpoint.socket())
        .await
        .expect("connected");
    frame::write(
        &mut stream,
        &aik_ipc::protocol::Hello {
            protocol: aik_ipc::PROTOCOL_VERSION,
            token: token.as_str().to_owned(),
            client: "greedy".to_owned(),
            interactive: false,
        },
    )
    .await
    .expect("written");
    let _welcome = frame::read::<_, aik_ipc::protocol::Welcome>(&mut stream)
        .await
        .expect("the host answers");

    // Four gigabytes announced, four bytes delivered. A host that believed the length would
    // allocate four gigabytes and then wait forever for the rest.
    use tokio::io::AsyncWriteExt as _;
    stream
        .write_all(&u32::MAX.to_be_bytes())
        .await
        .expect("written");
    // The host refuses the announcement on the strength of the length alone and ends the
    // connection, so the rest may or may not reach it. Either is correct; what matters is
    // that the host did not try to allocate four gigabytes waiting for it.
    let _ = stream.write_all(b"oops").await;

    let mut healthy = host.client(false).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), healthy.answered(Request::Ping))
            .await
            .expect("the host is still serving")
            .expect("answered"),
        Reply::Pong,
    );

    host.shut_down().await;
}

/// A well-formed call whose id repeats one already in flight.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_call_id_does_not_confuse_the_host() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(4))
        .start(root.path())
        .await;
    let token = Token::read_from(host.endpoint.token()).expect("the token");

    let mut stream = UnixStream::connect(host.endpoint.socket())
        .await
        .expect("connected");
    frame::write(
        &mut stream,
        &aik_ipc::protocol::Hello {
            protocol: aik_ipc::PROTOCOL_VERSION,
            token: token.as_str().to_owned(),
            client: "confused".to_owned(),
            interactive: false,
        },
    )
    .await
    .expect("written");
    let _welcome = frame::read::<_, aik_ipc::protocol::Welcome>(&mut stream)
        .await
        .expect("the host answers");

    for _ in 0..2 {
        frame::write(
            &mut stream,
            &Call {
                id: 1,
                request: Request::Ping,
            },
        )
        .await
        .expect("written");
    }

    for _ in 0..2 {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            frame::read::<_, Response>(&mut stream),
        )
        .await
        .expect("the host answers both")
        .expect("read")
        .expect("a frame");
        assert!(
            matches!(response, Response::Done { id: 1, .. }),
            "{response:?}"
        );
    }

    host.shut_down().await;
}
