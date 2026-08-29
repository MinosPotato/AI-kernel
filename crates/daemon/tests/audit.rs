//! Reading the trail through a host.
//!
//! While a host runs it holds the database, so a review has to go through it. The question
//! these tests answer is whether that changes anything about what a reader may see. It must
//! not: the store applies the same visibility rule, the reading identity is the same operator
//! it would be against the file, and nothing about the socket widens either.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_api::audit::{
    AuditEntry, AuditGap, AuditQuery, AuditRecord, AuditStore, AuthorizationDecided,
    AuthorizationOutcome, AuthorizationPhase,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, Principal, PrincipalId, PrincipalKind};
use aik_api::tool::ToolName;
use aik_audit::RedbAuditStore;
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use aik_ipc::protocol::{Reply, Request};
use aik_store::Db;
use support::{Answers, HostBuilder, Turn, permissive};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// One decision, attributed to `principal` acting for `on_behalf_of`.
fn decision(principal: &str, on_behalf_of: Option<&str>, tool: &str) -> AuditEntry {
    AuditEntry::Authorization(AuthorizationDecided {
        correlation: CorrelationId::new(),
        timestamp: Timestamp::now(),
        tool: ToolName::new(tool),
        principal: PrincipalId::new(principal),
        principal_kind: PrincipalKind::Agent,
        on_behalf_of: on_behalf_of.map(PrincipalId::new),
        action: ActionId::new("filesystem.read"),
        resource: None,
        scope_trust: None,
        phase: AuthorizationPhase::Tool,
        duration_ms: 0,
        approval_wait_ms: None,
        outcome: AuthorizationOutcome::Allowed,
    })
}

/// Writes entries straight into the trail, before any host opens the database.
async fn plant(database: &std::path::Path, entries: Vec<AuditEntry>) {
    let db = Arc::new(Db::open(database).expect("a database"));
    let store = RedbAuditStore::new(db).expect("an audit store");
    for entry in entries {
        store.append(entry).await.expect("appended");
    }
}

/// Everything the host will show `principal`, with no filter beyond a generous limit.
async fn review(host: &support::Host) -> Vec<AuditRecord> {
    let mut client = host.client(false).await;
    let reply = client
        .answered(Request::Audit {
            query: AuditQuery {
                limit: Some(500),
                ..AuditQuery::default()
            },
        })
        .await
        .expect("answered");
    match reply {
        Reply::Audit { records, .. } => records,
        other => panic!("the host answered the wrong shape: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// what a reader sees
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_client_reads_what_this_deployment_was_allowed_to_do() {
    let data = root();
    let root = root();
    std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");
    let database = data.path().join("aik.redb");

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

    // The sink writes on a subscriber, so the records arrive shortly after the call does.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let records = loop {
        let records = review(&host).await;
        if !records.is_empty() {
            break records;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the trail stayed empty"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert!(
        records
            .iter()
            .any(|record| record.entry.principal() == PrincipalId::new("assistant")),
        "the trail records the agent's own decisions: {records:?}",
    );
    // What it must not record is what was read.
    let rendered = format!("{records:?}");
    assert!(
        !rendered.contains("the file's contents"),
        "the trail carries the shape of what happened, never its contents: {rendered}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_principals_records_are_absent_however_they_are_asked_for() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    plant(
        &database,
        vec![
            decision("mallory", None, "filesystem.read"),
            decision("mallorys-agent", Some("mallory"), "filesystem.write"),
            decision("assistant", Some("user"), "filesystem.read"),
        ],
    )
    .await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    // Unfiltered, and then filtered by naming them explicitly. Naming somebody in a filter
    // narrows what a reader sees; it never widens it.
    for query in [
        AuditQuery {
            limit: Some(500),
            ..AuditQuery::default()
        },
        AuditQuery {
            principal: Some(PrincipalId::new("mallory")),
            limit: Some(500),
            ..AuditQuery::default()
        },
    ] {
        let Reply::Audit { records, .. } = client
            .answered(Request::Audit { query })
            .await
            .expect("answered")
        else {
            panic!("the host answered the wrong shape");
        };
        assert!(
            records.iter().all(
                |record| record.entry.principal() != PrincipalId::new("mallory")
                    && record.entry.principal() != PrincipalId::new("mallorys-agent")
            ),
            "a filter is not a way to widen visibility: {records:?}",
        );
    }

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reader_is_the_operator_rather_than_the_agent() {
    // The distinction is what makes a review useful: reading as the user shows the whole of
    // what that user's agents did *for them*, which is exactly the question somebody
    // reviewing an agent has.
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    plant(
        &database,
        vec![
            decision("assistant", Some("user"), "filesystem.read"),
            decision("assistant", Some("someone-else"), "filesystem.read"),
        ],
    )
    .await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;

    let records = review(&host).await;
    assert_eq!(
        records.len(),
        1,
        "only what was done on this operator's behalf: {records:?}",
    );
    let user = PrincipalId::new("user");
    assert_eq!(records[0].entry.on_behalf_of(), Some(&user));

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hole_in_the_trail_is_shown_to_every_reader() {
    // A trail may be incomplete; it may not lie about being complete. No identity and no
    // filter can hide the record that says so.
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    plant(
        &database,
        vec![
            AuditEntry::Gap(AuditGap {
                timestamp: Timestamp::now(),
                missed: 7,
            }),
            decision("mallory", None, "filesystem.read"),
        ],
    )
    .await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;

    let records = review(&host).await;
    assert!(
        records
            .iter()
            .any(|record| matches!(&record.entry, AuditEntry::Gap(gap) if gap.missed == 7)),
        "the record saying the trail is short must reach a reader who owns none of it: \
         {records:?}",
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// pruning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_prune_removes_nothing() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    plant(
        &database,
        vec![decision("assistant", Some("user"), "filesystem.read")],
    )
    .await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    let Reply::Pruned { removed, .. } = client
        .answered(Request::Prune {
            older_than_ms: 0,
            dry_run: true,
        })
        .await
        .expect("answered")
    else {
        panic!("the host answered the wrong shape");
    };
    assert_eq!(
        removed, 1,
        "the count is what makes a destructive step previewable"
    );

    assert_eq!(
        review(&host).await.len(),
        1,
        "a dry run must leave the trail exactly as it was",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prune_removes_records_and_records_that_it_did() {
    let data = root();
    let root = root();
    let database = data.path().join("aik.redb");

    plant(
        &database,
        vec![
            decision("assistant", Some("user"), "filesystem.read"),
            decision("assistant", Some("user"), "filesystem.write"),
        ],
    )
    .await;

    let host = HostBuilder::new()
        .database(&database)
        .start(root.path())
        .await;
    let mut client = host.client(false).await;

    let Reply::Pruned { removed, issued } = client
        .answered(Request::Prune {
            older_than_ms: 0,
            dry_run: false,
        })
        .await
        .expect("answered")
    else {
        panic!("the host answered the wrong shape");
    };
    assert_eq!(removed, 2);
    assert!(issued >= 2, "sequences are never renumbered: {issued}");

    let records = review(&host).await;
    assert!(
        records
            .iter()
            .any(|record| matches!(record.entry, AuditEntry::Retention(_))),
        "a log that can be truncated is only honest if the truncation is in the log: \
         {records:?}",
    );
    assert!(
        records
            .iter()
            .all(|record| !matches!(record.entry, AuditEntry::Authorization(_))),
        "the records asked for must actually be gone: {records:?}",
    );

    host.shut_down().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retention_period_that_reaches_before_time_is_refused() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;
    let mut client = host.client(false).await;

    let error = client
        .answered(Request::Prune {
            older_than_ms: u64::MAX,
            dry_run: true,
        })
        .await
        .expect_err("a period longer than the clock has run is not a period");
    assert_eq!(
        error.kind(),
        aik_core::ErrorKind::InvalidArgument,
        "{error}"
    );

    host.shut_down().await;
}

// ---------------------------------------------------------------------------
// there is still no way for a model to reach any of this
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_is_offered_no_tool_that_reaches_the_trail() {
    let root = root();
    let host = HostBuilder::new()
        .policy(permissive())
        .says([Turn::answer("nothing to see")])
        .start(root.path())
        .await;

    let mut client = host.client(false).await;
    client
        .answered(Request::Prompt {
            session: None,
            input: "what have you been allowed to do?".to_owned(),
        })
        .await
        .expect("answered");

    let offered: Vec<String> = host
        .model
        .requests()
        .first()
        .expect("the model was asked")
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(
        offered.iter().all(|name| !name.contains("audit")),
        "there is no audit tool, and there will not be one: {offered:?}",
    );

    host.shut_down().await;
}

/// The operator identity a review runs as, asserted against the resolved settings rather
/// than against a literal.
#[tokio::test(flavor = "multi_thread")]
async fn the_operator_is_the_user_and_the_conversation_principal_is_not() {
    let root = root();
    let host = HostBuilder::new().ephemeral().start(root.path()).await;

    let operator = host.settings.runtime.operator();
    let agent = host.settings.runtime.principal();

    assert_eq!(
        operator,
        Principal::new(host.settings.runtime.user.clone(), PrincipalKind::User)
    );
    assert_ne!(operator.id, agent.id);
    assert_eq!(operator.on_behalf_of, None);

    // And a review does not need an execution context built anywhere else to be right: the
    // one the host uses is derived from these settings and from nothing a client sent.
    let cx = ExecutionContext::new().with_principal(operator.clone());
    assert_eq!(cx.principal_or_system(), operator);

    host.shut_down().await;
}
