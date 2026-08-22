//! What the shipped frontend records, and what it refuses to hand to a model.
//!
//! Every test here starts [`aik_cli::wiring::builder`] — the production assembly, not a
//! rearrangement of it — and then asserts on the *durable trail*: what a real turn left
//! behind, who can read it, and what a restart still shows. A frontend that recorded nothing
//! would look completely normal on screen, which is why this is asserted from the store
//! rather than from the output.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_api::audit::{AuditEntryKind, AuditQuery, AuditStore};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalKind};
use aik_cli::args::{MemorySet, ToolSet};
use aik_cli::console::Console;
use aik_cli::session::Session;
use aik_store::Db;
use serde_json::json;
use support::{Harness, HarnessBuilder, Reply};

/// How long a test waits for the audit sink to catch up before failing rather than hanging.
const PATIENCE: Duration = Duration::from_secs(20);

/// A root for the filesystem tools and a separate directory for the database.
struct Workspace {
    root: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a temporary root");
        std::fs::write(root.path().join("notes.txt"), "the file's contents").expect("a file");
        Self {
            root,
            data: tempfile::tempdir().expect("a temporary data directory"),
        }
    }

    fn database(&self) -> std::path::PathBuf {
        self.data.path().join("aik.redb")
    }
}

/// Reading anything, with no questions asked.
fn allow_reads() -> serde_json::Value {
    json!([
        { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ])
}

/// Nothing at all is allowed, without a policy that says so.
fn deny_everything() -> serde_json::Value {
    json!([
        { "action": "filesystem.read", "effect": { "decision": "deny", "reason": "not here" } }
    ])
}

/// Runs one scripted turn that asks for a file, and stops the kernel.
async fn one_read(workspace: &Workspace, policy: serde_json::Value) {
    let harness = HarnessBuilder::new()
        .tools(ToolSet::ReadOnly)
        .memory(MemorySet::Off)
        .policy(policy)
        .database(workspace.database())
        .one_shot("read the notes")
        .reply(Reply::call(
            "c1",
            "filesystem.read",
            json!({ "path": "notes.txt" }),
        ))
        .reply(Reply::answer("done"))
        .build(workspace.root.path())
        .await;

    turn(&harness).await;
    harness.stop().await;
}

/// Drives the one scripted turn through the real session.
async fn turn(harness: &Harness) {
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session
        .one_shot("read the notes".to_owned())
        .await
        .expect("the turn runs");
}

/// Waits until the trail at `path` holds at least `count` records visible to `reader`.
async fn wait_for(
    path: &std::path::Path,
    reader: &str,
    count: usize,
) -> Vec<aik_api::audit::AuditRecord> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let found = read_trail(path, reader).await;
        if found.len() >= count {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the trail never reached {count} records; it held {}",
            found.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Opens the trail at `path` and reads it as `reader` would.
async fn read_trail(path: &std::path::Path, reader: &str) -> Vec<aik_api::audit::AuditRecord> {
    let db = Arc::new(Db::open(path).expect("the database reopens"));
    let store = aik_audit::RedbAuditStore::new(db).expect("the audit tables are there");
    let cx = ExecutionContext::new().with_principal(Principal::new(reader, PrincipalKind::User));
    store
        .query(&AuditQuery::default(), &cx)
        .await
        .expect("the trail can be read")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_turn_leaves_a_durable_record_of_what_was_allowed_and_what_ran() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;

    // Read after the kernel is gone: this is what an operator running `aik audit` tomorrow
    // sees, not what a subscriber saw while the process was alive.
    let records = read_trail(&workspace.database(), "alice").await;

    assert!(
        records
            .iter()
            .any(|record| record.entry.kind() == AuditEntryKind::Invocation
                && record.entry.tool().unwrap().as_str() == "filesystem.read"),
        "the invocation is missing from the trail: {records:#?}"
    );
    assert!(
        records
            .iter()
            .any(|record| record.entry.kind() == AuditEntryKind::Authorization),
        "the decision that permitted it is missing from the trail: {records:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trail_names_the_agent_and_the_user_it_acted_for() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;

    let records = read_trail(&workspace.database(), "alice").await;
    let invocation = records
        .iter()
        .find(|record| record.entry.kind() == AuditEntryKind::Invocation)
        .expect("an invocation");

    // The attribution a policy sees is the attribution the trail keeps: the agent is its own
    // actor, delegated to by the person. A trail that recorded the user as the actor would
    // make an autonomous action indistinguishable from one a human took.
    assert_eq!(invocation.entry.principal().as_str(), "assistant");
    assert_eq!(invocation.entry.principal_kind(), PrincipalKind::Agent);
    assert_eq!(
        invocation.entry.on_behalf_of().map(|id| id.as_str()),
        Some("alice")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_is_recorded_as_carefully_as_a_permission() {
    let workspace = Workspace::new();
    one_read(&workspace, deny_everything()).await;

    let records = read_trail(&workspace.database(), "alice").await;
    let refusals: Vec<_> = records
        .iter()
        .filter(|record| record.entry.is_refusal())
        .collect();
    assert!(
        !refusals.is_empty(),
        "a denied call left no refusal in the trail: {records:#?}"
    );
    assert!(
        refusals
            .iter()
            .any(|record| record.entry.kind() == AuditEntryKind::Authorization),
        "the decision that refused it is what an operator needs to see"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn another_user_cannot_read_this_run_s_trail() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;

    assert!(
        !read_trail(&workspace.database(), "alice").await.is_empty(),
        "the run's own user sees it"
    );
    assert!(
        read_trail(&workspace.database(), "mallory")
            .await
            .is_empty(),
        "somebody else with the same file open must not"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_tool_exposes_the_audit_trail_to_a_model() {
    // The single most important boundary in this subsystem, asserted where it can actually
    // be checked: against the catalogue the model is offered. A model that could read the
    // trail would be reading a map of where the boundaries are and where somebody has
    // already been refused; one that could write to it could describe its own behaviour.
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .tools(ToolSet::ReadWrite)
        .memory(MemorySet::Full)
        .policy(allow_reads())
        .database(workspace.database())
        .build(workspace.root.path())
        .await;

    // Against the registry, which is the only door onto a tool and the only list a model is
    // ever shown.
    let cx = ExecutionContext::new().with_principal(harness.settings.principal());
    let names: Vec<String> = harness
        .tools()
        .list(&cx)
        .await
        .expect("the catalogue can be listed")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();

    assert!(
        !names.iter().any(|name| name.starts_with("audit")),
        "the audit trail must not be reachable from a tool; found {names:?}"
    );
    assert!(
        !names.is_empty(),
        "the assertion above would pass vacuously against an empty catalogue"
    );

    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_run_is_still_audited_and_still_writes_nothing_to_disk() {
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .tools(ToolSet::ReadOnly)
        .memory(MemorySet::Off)
        .policy(allow_reads())
        .ephemeral()
        .one_shot("read the notes")
        .reply(Reply::call(
            "c1",
            "filesystem.read",
            json!({ "path": "notes.txt" }),
        ))
        .reply(Reply::answer("done"))
        .build(workspace.root.path())
        .await;

    let store = harness
        .kernel
        .context()
        .service::<dyn AuditStore>()
        .expect("an audit store is published even with no database");
    turn(&harness).await;

    let cx = ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User));
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let found = store.query(&AuditQuery::default(), &cx).await.unwrap();
        if !found.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "an ephemeral run recorded nothing at all"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    harness.stop().await;
    assert!(
        !workspace.database().exists(),
        "`--ephemeral` promises nothing reaches the disk, the trail included"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trail_survives_the_kernel_and_keeps_its_numbering() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;
    let first = read_trail(&workspace.database(), "alice").await;
    let highest = first.first().expect("records").sequence;

    // A second run against the same database continues the same trail rather than starting
    // a new one, which is what makes a break in the numbering evidence of tampering.
    one_read(&workspace, allow_reads()).await;
    let second = read_trail(&workspace.database(), "alice").await;

    assert!(second.len() > first.len());
    assert!(
        second.first().expect("records").sequence > highest,
        "the second run must continue the numbering, not restart it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_correlation_id_joins_a_decision_to_the_call_it_gated() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;

    let records = read_trail(&workspace.database(), "alice").await;
    let invocation = records
        .iter()
        .find(|record| record.entry.kind() == AuditEntryKind::Invocation)
        .expect("an invocation");
    let correlation = invocation.entry.correlation().expect("a correlation");

    let db = Arc::new(Db::open(workspace.database()).expect("the database reopens"));
    let store = aik_audit::RedbAuditStore::new(db).expect("the audit tables are there");
    let cx = ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User));
    let joined = store
        .query(
            &AuditQuery {
                correlation: Some(correlation),
                ..AuditQuery::default()
            },
            &cx,
        )
        .await
        .unwrap();

    assert!(
        joined.len() >= 2,
        "one operation should hold at least the decision and the call: {joined:#?}"
    );
    assert!(
        joined
            .iter()
            .all(|record| record.entry.correlation() == Some(correlation))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pruning_removes_records_and_says_so_in_the_trail() {
    let workspace = Workspace::new();
    one_read(&workspace, allow_reads()).await;
    let before = wait_for(&workspace.database(), "alice", 2).await.len();

    let db = Arc::new(Db::open(workspace.database()).expect("the database reopens"));
    let store = aik_audit::RedbAuditStore::new(db).expect("the audit tables are there");

    use aik_audit::AuditRetentionSweeper;
    let cutoff = aik_core::clock::Timestamp::from_millis(u64::MAX / 2);
    let previewed = store.count_older_than(cutoff).await.unwrap();
    assert_eq!(previewed, before, "the preview matches what is there");

    let removed = store.sweep_older_than(cutoff).await.unwrap();
    assert_eq!(removed, before);

    let cx = ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User));
    let after = store.query(&AuditQuery::default(), &cx).await.unwrap();
    assert_eq!(
        after.len(),
        1,
        "everything went except the record saying so: {after:#?}"
    );
    assert_eq!(after[0].entry.kind(), AuditEntryKind::Retention);
}
