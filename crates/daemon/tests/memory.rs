//! What a host remembers for one client and hands to the next.
//!
//! The claim under test is the whole point of a host process owning the database: a memory
//! recorded in one terminal session is reachable from another, later, over the same socket,
//! and the agent reaches it the only way anything reaches it — by asking for a tool, which
//! is authorized, dispatched and audited like every other call.
//!
//! Only the model is scripted, for the reason [`support`] gives. What is deliberately *not*
//! scripted is anything below it: the tool set the model is offered comes from the shipped
//! wiring, the query runs against the real store, and the result travels back through the
//! real registry.

mod support;

use aik_api::agent::AgentUpdate;
use aik_api::audit::{
    AuditEntry, AuditQuery, AuditRecord, AuthorizationOutcome, InvocationOutcome,
};
use aik_api::model::{ContentPart, Message, Role};
use aik_ipc::protocol::{Reply, Request, Response};
use serde_json::json;
use support::{Answers, HostBuilder, Turn, permissive};

/// A value no store could hold by coincidence, and that nothing in the fixture spells out.
const FACT: &str = "AIK_MEMORY_REGRESSION_739184";

/// The configuration this repository actually ships, which `docs/CLI.md` starts a host with.
fn shipped_config() -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cli")
        .join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("the example configuration is readable");
    serde_json::from_str(&text).expect("the example configuration is valid JSON")
}

/// Every text part of a message, joined.
fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether a message carries a successful tool result mentioning `needle`.
fn carries_tool_result(message: &Message, needle: &str) -> bool {
    message.content.iter().any(|part| match part {
        ContentPart::ToolResult {
            content, is_error, ..
        } => !is_error && content.to_string().contains(needle),
        _ => false,
    })
}

/// Runs one turn on a fresh session and returns every update the client saw.
async fn prompt(host: &support::Host, input: &str) -> Vec<AgentUpdate> {
    let mut client = host.client(true).await;
    let mut updates = Vec::new();
    let reply = client
        .call_observing(
            Request::Prompt {
                session: None,
                input: input.to_owned(),
            },
            |response| {
                if let Response::Update { update, .. } = response {
                    updates.push(update);
                }
            },
        )
        .await
        .expect("the host ran the turn");
    assert!(matches!(reply, Reply::Finished(_)), "{reply:?}");
    updates
}

// ---------------------------------------------------------------------------
// the instructions a deployment gives its agent reach a host
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_shipped_configurations_instructions_reach_the_model() {
    // The regression. The prompt lives in the deployment's own section, and a host that read
    // it from a section of its own found nothing there: the model was sent a window with no
    // system message, so nothing ever told it that its memory is durable and that nothing is
    // recalled from it automatically. The result looked like a broken memory and was a
    // missing sentence.
    let root = tempfile::tempdir().expect("a temporary root");
    let host = HostBuilder::new()
        .config(shipped_config())
        .says([Turn::answer("hello")])
        .start(root.path())
        .await;

    prompt(&host, "hello").await;

    let request = host
        .model
        .requests()
        .into_iter()
        .next()
        .expect("the model was asked for a completion");
    let system = request
        .messages
        .first()
        .expect("a window is never empty on a fresh session");
    assert_eq!(
        system.role,
        Role::System,
        "the deployment's instructions must be the first thing the model sees: {:?}",
        request.messages,
    );
    assert!(
        text_of(system).contains("memory.query"),
        "the shipped prompt is what tells the agent how to recall: {system:?}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_is_offered_an_executable_recall_tool_not_merely_a_mention_of_one() {
    // Being named in a prompt is not being callable. What decides whether the model can
    // actually recall is the tool set the shipped wiring registers, which is what this reads.
    let root = tempfile::tempdir().expect("a temporary root");
    let host = HostBuilder::new()
        .config(shipped_config())
        .says([Turn::answer("hello")])
        .start(root.path())
        .await;

    prompt(&host, "hello").await;

    let offered: Vec<String> = host
        .model
        .requests()
        .first()
        .expect("a completion")
        .tools
        .iter()
        .map(|definition| definition.name.to_string())
        .collect();

    for expected in ["memory.query", "memory.get", "memory.put"] {
        assert!(
            offered.contains(&expected.to_owned()),
            "`{expected}` must be an executable tool, not only a word in the prompt: {offered:?}",
        );
    }

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// one client writes, a later one recalls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_memory_one_client_recorded_is_recalled_by_a_later_one() {
    let root = tempfile::tempdir().expect("a temporary root");
    let host = HostBuilder::new()
        .config(shipped_config())
        .says([
            // The first client's turn: the model records what it was told.
            Turn::call(
                "put",
                "memory.put",
                json!({ "kind": "fact", "content": FACT }),
            ),
            Turn::answer("noted"),
            // The second client's turn: a query naming no value, only a kind. Nothing here
            // carries the fact itself, so the only way it can come back is out of the store.
            Turn::call("query", "memory.query", json!({ "kinds": ["fact"] })),
            Turn::answer("recalled"),
        ])
        .start(root.path())
        .await;

    // Client A, which then disconnects.
    prompt(&host, "remember what I just told you").await;

    // Client B: a new connection and a new session, sharing only the host's database.
    let updates = prompt(&host, "what did I ask you to remember?").await;

    let called = updates.iter().any(|update| {
        matches!(update, AgentUpdate::ToolCall(call) if call.name.as_str() == "memory.query")
    });
    assert!(called, "the retrieval must be a tool call: {updates:?}");

    let recalled = updates.iter().any(|update| match update {
        AgentUpdate::ToolResult { outcome, .. } => {
            !outcome.is_error && outcome.output.to_string().contains(FACT)
        }
        _ => false,
    });
    assert!(
        recalled,
        "the query must return the record the earlier client stored: {updates:?}",
    );

    // And the model was actually handed it: a result the loop never fed back would be a
    // retrieval the agent could not answer from.
    let last = host
        .model
        .requests()
        .pop()
        .expect("the model was asked again after the tool ran");
    assert!(
        last.messages
            .iter()
            .any(|message| message.role == Role::Tool && carries_tool_result(message, FACT)),
        "the tool result must reach the model: {:?}",
        last.messages,
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recall_is_accounted_for_in_the_audit_trail() {
    let root = tempfile::tempdir().expect("a temporary root");
    let host = HostBuilder::new()
        .policy(permissive())
        .says([
            Turn::call(
                "put",
                "memory.put",
                json!({ "kind": "fact", "content": FACT }),
            ),
            Turn::answer("noted"),
            Turn::call("query", "memory.query", json!({ "kinds": ["fact"] })),
            Turn::answer("recalled"),
        ])
        .start(root.path())
        .await;

    prompt(&host, "remember it").await;
    prompt(&host, "recall it").await;

    // The trail is written from the event bus, so a read taken the instant a turn finishes
    // can legitimately be a record or two behind it. Polled to a deadline rather than slept
    // on, so the test is bounded without being timing-dependent.
    let records = await_recall(&host).await;

    let invoked = records.iter().any(|record| {
        matches!(&record.entry, AuditEntry::Invocation(event)
            if event.tool.as_str() == "memory.query"
                && event.outcome == InvocationOutcome::Succeeded)
    });
    assert!(
        invoked,
        "a recall is an invocation like any other: {records:?}"
    );

    let authorized = records.iter().any(|record| {
        matches!(&record.entry, AuditEntry::Authorization(event)
            if event.action.as_str() == "memory.query"
                && event.outcome == AuthorizationOutcome::Allowed)
    });
    assert!(
        authorized,
        "a recall is authorized host-side, and the trail must say so: {records:?}",
    );

    host.shut_down().await;
}

/// Reads the trail until it holds the recall's own invocation record, or gives up.
async fn await_recall(host: &support::Host) -> Vec<AuditRecord> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut client = host.client(false).await;
    loop {
        let Reply::Audit { records, .. } = client
            .answered(Request::Audit {
                query: AuditQuery {
                    limit: Some(500),
                    ..AuditQuery::default()
                },
            })
            .await
            .expect("the host answered")
        else {
            panic!("the host answered the wrong shape");
        };

        let landed = records.iter().any(|record| {
            matches!(&record.entry, AuditEntry::Invocation(event)
                if event.tool.as_str() == "memory.query")
        });
        if landed || std::time::Instant::now() >= deadline {
            return records;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
