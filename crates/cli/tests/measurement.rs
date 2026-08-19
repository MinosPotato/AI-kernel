//! Measurement events and JSONL recording, exercised through the real wiring.
//!
//! Only the model is scripted — see `support` — so what these tests check is what the real
//! agent loop, tool registry and context store actually publish and what the real
//! [`Recorder`](aik_cli::recorder::Recorder) actually writes, not a stand-in for either.

mod support;

use std::io::BufRead as _;

use aik_api::agent::SessionId;
use aik_api::measurement::RequestMeasured;
use aik_cli::console::Console;
use aik_cli::recorder::Recorder;
use aik_cli::session::Session;
use serde_json::Value;
use support::{HarnessBuilder, Reply};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn read_records(path: &std::path::Path) -> Vec<Value> {
    let file = std::fs::File::open(path).expect("the recording file exists");
    std::io::BufReader::new(file)
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn a_request_measured_event_is_published_for_a_plain_turn() {
    let root = root();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("hello"))
        .build(root.path())
        .await;
    let mut measurements = harness.kernel.context().subscribe::<RequestMeasured>();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"hi\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    session.interactive().await.unwrap();

    let mut events = Vec::new();
    while let Some(Ok(envelope)) = measurements.try_recv() {
        events.push(envelope.payload);
    }
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].turn, 1);
    assert_eq!(
        events[0].estimate.tools_offered, 2,
        "read + list by default"
    );
    // The scripted provider in this harness never reports usage — see `Reply::into_response`
    // — so provider usage must be absent rather than fabricated.
    assert!(events[0].provider_usage.is_none());

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn measurement_does_not_change_what_the_model_is_sent_or_what_runs() {
    // The point of an observational layer: build two otherwise-identical harnesses, drive
    // one with a subscriber attached and one without, and check the model saw the same
    // request either way.
    let root_a = root();
    let root_b = root();
    let harness_a = HarnessBuilder::new()
        .reply(Reply::answer("hello"))
        .build(root_a.path())
        .await;
    let harness_b = HarnessBuilder::new()
        .reply(Reply::answer("hello"))
        .build(root_b.path())
        .await;

    // Only `harness_a` has a subscriber pulling `RequestMeasured` off the bus.
    let _measurements = harness_a.kernel.context().subscribe::<RequestMeasured>();

    for harness in [&harness_a, &harness_b] {
        let mut session = Session::new(
            &harness.kernel.context(),
            &harness.settings,
            Console::new(&b"hi\n/quit\n"[..]),
            None,
        )
        .expect("a session");
        session.interactive().await.unwrap();
    }

    let sent_a = &harness_a.model.requests()[0];
    let sent_b = &harness_b.model.requests()[0];
    assert_eq!(sent_a.messages, sent_b.messages);
    assert_eq!(sent_a.tools, sent_b.tools);

    harness_a.kernel.shutdown().await.unwrap();
    harness_b.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_recorder_writes_one_line_per_measured_turn_and_no_message_content() {
    let root = root();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("the secret answer is 42"))
        .build(root.path())
        .await;

    let record_path = root.path().join("run.jsonl");
    let recorder = Recorder::create(&record_path).expect("opens for appending");

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"what is the secret?\n/quit\n"[..]),
        None,
    )
    .expect("a session")
    .with_recorder(recorder);
    session.interactive().await.unwrap();

    let records = read_records(&record_path);
    assert!(!records.is_empty());
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "request_measured")
    );

    let rendered = std::fs::read_to_string(&record_path).unwrap();
    assert!(
        !rendered.contains("secret"),
        "the recording must never carry prompt or response text: {rendered}"
    );

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_invalid_recording_destination_is_reported_rather_than_silently_ignored() {
    let error = Recorder::create(std::path::Path::new("/nonexistent/deeply/nested/run.jsonl"))
        .expect_err("no such directory exists");
    assert!(error.to_string().contains("run.jsonl"), "{error}");
}

#[tokio::test]
async fn session_totals_accumulate_across_more_than_one_prompt() {
    let root = root();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("one"))
        .reply(Reply::answer("two"))
        .build(root.path())
        .await;

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"first\nsecond\n/quit\n"[..]),
        None,
    )
    .expect("a session");
    session.interactive().await.unwrap();

    let totals = session.session_stats();
    assert_eq!(totals.turns, 2, "one model turn per prompt, in this script");

    harness.kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn recorded_sessions_do_not_cross_between_independent_sessions() {
    let root = root();
    let harness = HarnessBuilder::new()
        .reply(Reply::answer("first answer"))
        .reply(Reply::answer("second answer"))
        .build(root.path())
        .await;

    let record_path = root.path().join("run.jsonl");
    let recorder = Recorder::create(&record_path).unwrap();

    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b"first\n/new\nsecond\n/quit\n"[..]),
        None,
    )
    .expect("a session")
    .with_recorder(recorder);
    session.interactive().await.unwrap();

    let records = read_records(&record_path);
    let sessions: std::collections::HashSet<&str> = records
        .iter()
        .filter(|record| record["event"] == "request_measured")
        .map(|record| record["session"].as_str().unwrap())
        .collect();
    assert_eq!(
        sessions.len(),
        2,
        "the two prompts belong to different sessions"
    );

    let _ = SessionId::new(); // keep the import meaningful if the assertion above changes
    harness.kernel.shutdown().await.unwrap();
}
