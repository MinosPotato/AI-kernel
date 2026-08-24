//! Work nobody is watching.
//!
//! The schedule was durable before this crate existed; what was missing was a process that is
//! always there to run it, and something for a firing to do. These tests are about both, and
//! about the part that matters most precisely because nobody is looking: a firing is still
//! gated, still owned, and still cannot be told whose authority to carry.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_api::context::ContextStore;
use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_api::scheduler::{JobId, Trigger};
use aik_context::RedbContextStore;
use aik_core::ErrorKind;
use aik_ipc::protocol::{Reply, Request, ScheduleRequest};
use aik_store::Db;
use support::{Answers, Host, HostBuilder, Turn, permissive};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn answers(count: usize) -> Vec<Turn> {
    (0..count)
        .map(|n| Turn::answer(&format!("answer {n}")))
        .collect()
}

fn every(interval: Duration) -> Trigger {
    Trigger::Every { interval }
}

/// Waits until the model has been asked at least `count` times.
async fn await_completions(host: &Host, count: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while host.model.completions() < count {
        assert!(
            std::time::Instant::now() < deadline,
            "the model was asked {} times, not {count}",
            host.model.completions(),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// a job actually runs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_host_says_that_it_runs_the_schedule() {
    // The one status field a client acts on: with it false, a job accepted here would be a
    // job that never fires, and the host refuses to accept one at all.
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let Reply::Status(status) = client.answered(Request::Status).await.expect("answered") else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        status.runs_jobs,
        "this is the process that runs unattended work; it has to say so",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_job_runs_an_agent_turn_in_the_host() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(8))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let reply = client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("nightly"),
            trigger: every(Duration::from_millis(50)),
            prompt: "summarise the day".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect("scheduled");
    assert_eq!(reply, Reply::Ok);

    await_completions(&host, 1).await;

    let sent = format!("{:?}", host.model.requests());
    assert!(
        sent.contains("summarise the day"),
        "a firing has to actually ask the agent the job's prompt: {sent}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_job_survives_a_restart_and_keeps_firing() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");
    let socket = root.path().join("run").join("aikd.sock");

    {
        let host = HostBuilder::new()
            .database(&database)
            .socket(&socket)
            .policy(permissive())
            .says(answers(8))
            .start(root.path())
            .await;

        let mut client = host.client(false).await;
        client
            .answered(Request::Schedule(ScheduleRequest {
                id: JobId::new("nightly"),
                trigger: every(Duration::from_millis(50)),
                prompt: "the job that outlives the process".to_owned(),
                session: None,
                persistent: true,
                timeout_ms: None,
            }))
            .await
            .expect("scheduled");

        await_completions(&host, 1).await;
        host.shut_down().await;
    }

    let host = HostBuilder::new()
        .database(&database)
        .socket(&socket)
        .policy(permissive())
        .says(answers(8))
        .start(root.path())
        .await;

    // A fresh model: anything it is asked was asked by the *new* process.
    await_completions(&host, 1).await;
    let sent = format!("{:?}", host.model.requests());
    assert!(
        sent.contains("the job that outlives the process"),
        "a persistent job has to keep firing after a restart: {sent}",
    );

    let mut client = host.client(false).await;
    let Reply::Jobs(jobs) = client.answered(Request::Jobs).await.expect("listed") else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        jobs.iter().any(|job| job.spec.id == JobId::new("nightly")),
        "the definition survives too, not merely its effects: {jobs:?}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_volatile_job_is_refused_by_a_host_with_nowhere_to_put_it() {
    // The scheduler's own rule, unchanged: a persistent job asked of a scheduler that has no
    // store is refused rather than accepted and forgotten.
    let root = root();
    let host = HostBuilder::new()
        .ephemeral()
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let error = client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("nightly"),
            trigger: every(Duration::from_secs(3600)),
            prompt: "this cannot be kept".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect_err("a job that cannot be kept must not be accepted");
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// whose authority a firing carries
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_job_belongs_to_the_principal_that_scheduled_it() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(8))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("mine"),
            trigger: every(Duration::from_secs(3600)),
            prompt: "later".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect("scheduled");

    let Reply::Jobs(jobs) = client.answered(Request::Jobs).await.expect("listed") else {
        panic!("the host answered the wrong shape");
    };
    let job = jobs.first().expect("the job just scheduled");
    assert_eq!(
        job.owner,
        host.settings.runtime.principal().id,
        "the owner is stamped from the connection's principal, never from the request",
    );
    assert_eq!(
        job.spec.handler.as_str(),
        aik_runtime::jobs::DEFAULT_COMPONENT_ID,
        "a client says when and what to ask; what runs it is the host's decision",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_firing_acts_for_the_owner_without_becoming_them() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    {
        let host = HostBuilder::new()
            .database(&database)
            .policy(permissive())
            .says(answers(8))
            .start(root.path())
            .await;

        let mut client = host.client(false).await;
        client
            .answered(Request::Schedule(ScheduleRequest {
                id: JobId::new("nightly"),
                trigger: every(Duration::from_millis(50)),
                prompt: "unattended".to_owned(),
                session: None,
                persistent: true,
                timeout_ms: None,
            }))
            .await
            .expect("scheduled");

        await_completions(&host, 1).await;
        host.shut_down().await;
    }

    // Read straight from the database, because this is the one identity no client can see:
    // it is not the agent, and it is not the user the agent acts for.
    let db = Arc::new(Db::open(&database).expect("the database is released"));
    let store = RedbContextStore::new(db).expect("a context store");

    let firing = Principal::new("scheduler", PrincipalKind::System).on_behalf_of("assistant");
    let cx = ExecutionContext::new().with_principal(firing);
    let sessions = store.sessions(&cx).await.expect("listed");

    let unattended: Vec<_> = sessions
        .iter()
        .filter(|stats| stats.owner == PrincipalId::new("scheduler"))
        .collect();
    assert!(
        !unattended.is_empty(),
        "a firing's transcript belongs to the scheduler acting for the owner: {sessions:?}",
    );
    assert!(
        !sessions
            .iter()
            .any(|stats| stats.owner == PrincipalId::new("user")),
        "a firing must never own anything as the human: {sessions:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_firings_transcript_is_not_the_clients_to_read() {
    // The other side of the same coin: because a firing runs as `scheduler`, the sessions it
    // creates are not visible to the agent principal a client connects as.
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    let host = HostBuilder::new()
        .database(&database)
        .policy(permissive())
        .says(answers(8))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("nightly"),
            trigger: every(Duration::from_millis(50)),
            prompt: "unattended".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect("scheduled");
    await_completions(&host, 1).await;

    let Reply::Sessions(sessions) = client.answered(Request::Sessions).await.expect("listed")
    else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        sessions
            .iter()
            .all(|stats| stats.owner != PrincipalId::new("scheduler")),
        "a client sees what its own principal may act for, and a firing is not that: {sessions:?}",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// a firing is still gated
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_firings_tool_call_is_refused_when_no_policy_allows_it() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    // No policy at all, which denies everything — including at 3am with nobody watching.
    let host = HostBuilder::new()
        .says([
            Turn::call(
                "c1",
                "filesystem.read",
                serde_json::json!({ "path": "notes.txt" }),
            ),
            Turn::answer("I could not read it"),
            Turn::answer("nor now"),
            Turn::answer("nor now"),
        ])
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("nightly"),
            trigger: every(Duration::from_millis(50)),
            prompt: "read notes.txt".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect("scheduled");

    await_completions(&host, 2).await;

    let sent = format!("{:?}", host.model.requests());
    assert!(
        !sent.contains("the file's contents"),
        "an unattended firing is not a way around the policy engine: {sent}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_firing_that_needs_a_human_is_refused_when_no_client_can_answer() {
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");

    let host = HostBuilder::new()
        .policy(support::ask_per_file())
        .says([
            Turn::call(
                "c1",
                "filesystem.read",
                serde_json::json!({ "path": "notes.txt" }),
            ),
            Turn::answer("I could not read it"),
            Turn::answer("nor now"),
            Turn::answer("nor now"),
        ])
        .start(root.path())
        .await;

    // Scheduled by a client that is not interactive, and then goes away entirely.
    {
        let mut client = host.client(false).await;
        client
            .answered(Request::Schedule(ScheduleRequest {
                id: JobId::new("nightly"),
                trigger: every(Duration::from_millis(50)),
                prompt: "read notes.txt".to_owned(),
                session: None,
                persistent: true,
                timeout_ms: None,
            }))
            .await
            .expect("scheduled");
    }

    await_completions(&host, 2).await;

    let sent = format!("{:?}", host.model.requests());
    assert!(
        !sent.contains("the file's contents"),
        "an approval with nobody to ask must be a refusal, not a default yes: {sent}",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// cancelling
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_job_stops_firing_and_stays_cancelled() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");
    let socket = root.path().join("run").join("aikd.sock");

    {
        let host = HostBuilder::new()
            .database(&database)
            .socket(&socket)
            .policy(permissive())
            .says(answers(16))
            .start(root.path())
            .await;

        let mut client = host.client(false).await;
        client
            .answered(Request::Schedule(ScheduleRequest {
                id: JobId::new("nightly"),
                trigger: every(Duration::from_millis(50)),
                prompt: "for now".to_owned(),
                session: None,
                persistent: true,
                timeout_ms: None,
            }))
            .await
            .expect("scheduled");
        await_completions(&host, 1).await;

        let reply = client
            .answered(Request::CancelJob {
                job: JobId::new("nightly"),
            })
            .await
            .expect("cancelled");
        assert_eq!(reply, Reply::Cancelled { existed: true });

        host.shut_down().await;
    }

    let host = HostBuilder::new()
        .database(&database)
        .socket(&socket)
        .policy(permissive())
        .says(answers(16))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let Reply::Jobs(jobs) = client.answered(Request::Jobs).await.expect("listed") else {
        panic!("the host answered the wrong shape");
    };
    assert!(
        jobs.is_empty(),
        "a cancellation that returned true means the job is gone after a restart too: {jobs:?}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_job_that_was_never_scheduled_says_so_rather_than_failing() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    assert_eq!(
        client
            .answered(Request::CancelJob {
                job: JobId::new("never"),
            })
            .await
            .expect("answered"),
        Reply::Cancelled { existed: false },
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_with_nothing_to_ask_is_refused() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says(answers(2))
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    let error = client
        .answered(Request::Schedule(ScheduleRequest {
            id: JobId::new("empty"),
            trigger: every(Duration::from_secs(3600)),
            prompt: "   ".to_owned(),
            session: None,
            persistent: true,
            timeout_ms: None,
        }))
        .await
        .expect_err("a job that asks nothing is not a job");
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");

    host.shut_down().await;
}
