//! What the shipped frontend does with a database in it.
//!
//! Every test here starts [`aik_cli::wiring::builder`] — the production assembly, not a
//! rearrangement of it — over a temporary database, and asserts on what survived, what was
//! refused, and what was released. The only stub is the model, for the reason
//! [`support`] gives: a language model that reliably calls the tool a test is about does not
//! exist.
//!
//! # Why none of this can touch a real database
//!
//! Three independent things would each have to fail before one of these tests wrote to the
//! operator's own store. The path is always passed explicitly, as `--db`; the environment
//! handed to `Settings::resolve_from` is empty, so the XDG default has nothing to resolve
//! *from* and refuses rather than guessing; and redb takes an exclusive lock, so a test that
//! somehow reached a live database would fail to open it rather than write to it.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_api::context::{ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryQuery, MemoryStore};
use aik_api::model::{Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::scheduler::{JobHandler, JobSpec, Scheduler, Trigger};
use aik_api::tool::{ToolName, ToolRegistry};
use aik_cli::args::MemorySet;
use aik_cli::console::Console;
use aik_cli::session::Session;
use aik_cli::settings::{DATABASE_PATH_KEY, Storage};
use aik_core::prelude::*;
use aik_store::Db;
use serde_json::{Value, json};
use support::{Harness, HarnessBuilder, Reply};

/// How long a test waits for something it expects before failing rather than hanging.
const PATIENCE: Duration = Duration::from_secs(20);

/// A root for the filesystem tools and a separate directory for the database.
///
/// Separate on purpose: a database inside the directory the agent's filesystem tools are
/// confined to would be a file the agent could read, which is not how a deployment is
/// arranged and not something a test should quietly normalise.
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

    /// The one database every run in this workspace opens.
    fn database(&self) -> std::path::PathBuf {
        self.data.path().join("aik.redb")
    }

    /// Starts a run over this workspace's database.
    async fn open(&self, builder: HarnessBuilder) -> Harness {
        builder
            .database(self.database())
            .build(self.root.path())
            .await
    }
}

/// Everything a memory tool needs, at both authorization phases.
fn allow_all_memory() -> Value {
    json!([
        { "action": "memory.*", "effect": { "decision": "allow" } },
        { "action": "memory.*", "resource": "*", "effect": { "decision": "allow" } }
    ])
}

/// The shape the shipped policy uses: recall freely, record only under known kinds.
fn allow_known_kinds() -> Value {
    json!([
        { "action": "memory.get", "effect": { "decision": "allow" } },
        { "action": "memory.get", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "memory.query", "effect": { "decision": "allow" } },
        { "action": "memory.query", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "memory.put", "effect": { "decision": "allow" } },
        { "action": "memory.put", "resource": "kind/note", "effect": { "decision": "allow" } }
    ])
}

/// Stores one memory through the registry, as a turn would.
async fn remember(
    harness: &Harness,
    kind: &str,
    content: &str,
) -> Result<aik_api::tool::ToolOutcome> {
    harness
        .tools()
        .invoke(
            &ToolName::new("memory.put"),
            json!({ "kind": kind, "content": content }),
            &harness.cx(),
        )
        .await
}

/// Everything the run's principal can see, by kind.
async fn recall(harness: &Harness, kind: &str) -> Vec<Value> {
    let outcome = harness
        .tools()
        .invoke(
            &ToolName::new("memory.query"),
            json!({ "kinds": [kind] }),
            &harness.cx(),
        )
        .await
        .expect("querying is allowed");
    assert!(!outcome.is_error, "query failed: {:?}", outcome.output);
    outcome.output["records"]
        .as_array()
        .expect("a records array")
        .clone()
}

// ---------------------------------------------------------------------------
// 1. the frontend starts with the whole durable stack in it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_frontend_starts_with_every_durable_subsystem_published() {
    let workspace = Workspace::new();
    let harness = workspace.open(HarnessBuilder::new()).await;
    let kernel = harness.kernel.context();

    // Resolved by capability rather than by type, exactly as a dependant would: what a
    // subsystem is wired to is what it can find in the registry, not what the wiring
    // intended to put there.
    assert!(kernel.service::<Db>().is_ok(), "the shared database");
    assert!(kernel.service::<dyn ContextStore>().is_ok(), "transcripts");
    assert!(kernel.service::<dyn MemoryStore>().is_ok(), "memories");
    assert!(kernel.service::<dyn Scheduler>().is_ok(), "the schedule");
    assert!(kernel.service::<dyn ToolRegistry>().is_ok(), "tools");

    let ids: Vec<String> = harness
        .kernel
        .component_ids()
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    for expected in [
        aik_store::DEFAULT_COMPONENT_ID,
        aik_context::DEFAULT_COMPONENT_ID,
        aik_memory::DEFAULT_COMPONENT_ID,
        aik_memory::DEFAULT_TOOLS_COMPONENT_ID,
        aik_scheduler::DEFAULT_COMPONENT_ID,
    ] {
        assert!(
            ids.contains(&expected.to_owned()),
            "{expected} is missing from {ids:?}"
        );
    }

    harness.stop().await;
}

#[tokio::test]
async fn an_ephemeral_run_opens_no_database_but_still_holds_all_three_subsystems() {
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .ephemeral()
        .build(workspace.root.path())
        .await;

    assert_eq!(harness.settings.storage, Storage::Ephemeral);
    assert!(
        harness.kernel.context().service::<Db>().is_err(),
        "an ephemeral run must not have opened a database",
    );
    // The capabilities are still there — only their durability changed. Nothing downstream
    // is allowed to notice which backend it got.
    let kernel = harness.kernel.context();
    assert!(kernel.service::<dyn ContextStore>().is_ok());
    assert!(kernel.service::<dyn MemoryStore>().is_ok());
    assert!(kernel.service::<dyn Scheduler>().is_ok());

    assert!(
        !workspace.database().exists(),
        "nothing may be written to the configured path when --ephemeral was asked for",
    );

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// 2. the configured database is the one that gets used
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_configured_database_is_the_file_that_is_actually_opened() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().policy(allow_all_memory()))
        .await;

    let path = workspace.database();
    assert_eq!(harness.settings.database(), Some(path.as_path()));
    assert!(path.exists(), "the store opened something, somewhere else");
    assert_eq!(
        harness.settings.config.value(DATABASE_PATH_KEY),
        Some(&json!(path.to_string_lossy())),
        "the path the frontend reported must be the path the component was handed",
    );

    // A memory written now has to land in that file and nowhere else, which is only
    // provable by reading it back out of the file after the kernel that wrote it is gone.
    remember(&harness, "note", "in the configured file")
        .await
        .expect("the tool runs");
    harness.stop().await;

    let db = Db::open(&path).expect("the database is a real one at the configured path");
    assert_eq!(
        db.schema_version().expect("a schema version"),
        aik_store::SCHEMA_VERSION
    );
}

#[tokio::test]
async fn a_configuration_file_names_the_database_through_the_component_it_belongs_to() {
    // The nesting is the hazard: the component is `store.db`, so its settings live at
    // `components.store.db`, with the dot in the id becoming another level of object. A
    // document that wrote `components["store.db"]` instead would parse, start, and open the
    // XDG default — a silent fallback onto the operator's real database, which is exactly
    // the failure this asserts against.
    let workspace = Workspace::new();
    let path = workspace.database();
    let harness = HarnessBuilder::new()
        .policy(allow_all_memory())
        .config(json!({
            "components": { "store": { "db": { "path": path } } }
        }))
        .build(workspace.root.path())
        .await;

    assert_eq!(harness.settings.database(), Some(path.as_path()));
    remember(&harness, "note", "named by configuration")
        .await
        .expect("the tool runs");
    harness.stop().await;

    assert!(path.exists(), "the configured path was not the one used");
    let restarted = HarnessBuilder::new()
        .policy(allow_all_memory())
        .config(json!({
            "components": { "store": { "db": { "path": path } } }
        }))
        .build(workspace.root.path())
        .await;
    assert_eq!(recall(&restarted, "note").await.len(), 1);
    restarted.stop().await;
}

#[tokio::test]
async fn the_command_line_overrides_the_configured_database() {
    let workspace = Workspace::new();
    let elsewhere = tempfile::tempdir().expect("a second data directory");
    let flagged = elsewhere.path().join("flagged.redb");

    let harness = HarnessBuilder::new()
        .policy(allow_all_memory())
        .config(json!({
            "components": { "store": { "db": { "path": workspace.database() } } }
        }))
        .database(flagged.clone())
        .build(workspace.root.path())
        .await;

    assert_eq!(harness.settings.database(), Some(flagged.as_path()));
    remember(&harness, "note", "in the flagged file")
        .await
        .expect("the tool runs");
    harness.stop().await;

    assert!(flagged.exists());
    assert!(
        !workspace.database().exists(),
        "the configured path must not have been opened as well",
    );
}

#[tokio::test]
async fn the_database_file_is_the_owners_alone() {
    // The store's own guarantee, asserted here because the frontend is what chooses the
    // path: a transcript store created world-readable is a privacy failure whatever the
    // policy says about tools.
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = Workspace::new();
    let harness = workspace.open(HarnessBuilder::new()).await;
    let path = workspace.database();

    let mode = std::fs::metadata(&path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, aik_store::DATABASE_FILE_MODE, "mode {mode:o}");

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// 3. the transcript survives a restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_conversation_is_still_there_after_a_restart() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().reply(Reply::answer("the first answer")))
        .await;

    // A real turn, through the frontend's own session type, so what is stored is what the
    // shipped path stores rather than what this test decided to append.
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session
        .one_shot("what is in notes.txt?".to_owned())
        .await
        .expect("the turn runs");
    let id = session.id();
    let cx = harness.cx();

    let before = harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a context store")
        .stats(&id, &cx)
        .await
        .expect("stats")
        .expect("the session exists");
    assert!(before.records > 0, "the turn wrote nothing");

    drop(session);
    harness.stop().await;

    let restarted = workspace.open(HarnessBuilder::new()).await;
    let after = restarted
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a context store")
        .stats(&id, &cx)
        .await
        .expect("stats")
        .expect("the session survived the restart");
    assert_eq!(after.records, before.records);

    let window = restarted
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a context store")
        .window(&id, &Default::default(), &cx)
        .await
        .expect("a window");
    assert!(
        !window.messages.is_empty(),
        "the transcript came back empty after a restart",
    );

    restarted.stop().await;
}

#[tokio::test]
async fn an_ephemeral_run_forgets_the_conversation() {
    // The other half of the same claim: durability is a property of the backend, and the
    // volatile one has to actually be volatile or the choice means nothing.
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .ephemeral()
        .build(workspace.root.path())
        .await;

    let id = aik_api::agent::SessionId::new();
    let cx = harness.cx();
    harness
        .kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("a context store")
        .append(
            &id,
            ContextEntry::new(Message::text(Role::User, "hello")),
            &cx,
        )
        .await
        .expect("appended");
    harness.stop().await;

    let restarted = HarnessBuilder::new()
        .ephemeral()
        .build(workspace.root.path())
        .await;
    assert!(
        restarted
            .kernel
            .context()
            .service::<dyn ContextStore>()
            .expect("a context store")
            .stats(&id, &cx)
            .await
            .expect("stats")
            .is_none(),
        "an ephemeral run kept something across a restart",
    );
    restarted.stop().await;
}

// ---------------------------------------------------------------------------
// 4. memories survive a restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_memory_written_through_a_tool_is_still_there_after_a_restart() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().policy(allow_all_memory()))
        .await;

    let outcome = remember(&harness, "note", "the kettle is in the second cupboard")
        .await
        .expect("the tool runs");
    assert!(!outcome.is_error, "{:?}", outcome.output);
    harness.stop().await;

    let restarted = workspace
        .open(HarnessBuilder::new().policy(allow_all_memory()))
        .await;
    let records = recall(&restarted, "note").await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0]["content"],
        json!("the kettle is in the second cupboard")
    );

    restarted.stop().await;
}

// ---------------------------------------------------------------------------
// 5. scheduled jobs survive a restart
// ---------------------------------------------------------------------------

/// A handler that records its firings and, optionally, writes a memory while it runs.
struct Job {
    fired: std::sync::atomic::AtomicUsize,
    remember: bool,
    store: std::sync::OnceLock<Arc<dyn MemoryStore>>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").finish_non_exhaustive()
    }
}

impl Job {
    fn new(remember: bool) -> Arc<Self> {
        Arc::new(Self {
            fired: std::sync::atomic::AtomicUsize::new(0),
            remember,
            store: std::sync::OnceLock::new(),
        })
    }

    fn firings(&self) -> usize {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl JobHandler for Job {
    async fn run(&self, _job: &JobSpec, cx: &ExecutionContext) -> Result<()> {
        if self.remember {
            let store = self.store.get().expect("init ran before the first firing");
            let record = aik_api::memory::MemoryRecord::new(
                "job",
                json!("written by a firing"),
                aik_core::clock::Timestamp::now(),
            );
            store.put(record, cx).await?;
        }
        self.fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Publishes a [`Job`] under a component id a job spec can name.
#[derive(Debug)]
struct JobComponent {
    id: ComponentId,
    handler: Arc<Job>,
}

impl JobComponent {
    fn new(id: &str, handler: Arc<Job>) -> Arc<Self> {
        Arc::new(Self {
            id: ComponentId::new(id),
            handler,
        })
    }
}

#[async_trait]
impl Component for JobComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        // Declared, so the store is open before a firing can reach it — the same rule every
        // durable subsystem follows, and the reason this is a component at all.
        ComponentDescriptor::new(self.id.clone()).requires(aik_memory::DEFAULT_COMPONENT_ID)
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        if self.handler.remember {
            let _ = self.handler.store.set(ctx.service::<dyn MemoryStore>()?);
        }
        ctx.provide::<dyn JobHandler>(self.handler.clone())
    }
}

/// A job that fires far enough in the future that no test races it.
fn dormant(id: &str, handler: &str) -> JobSpec {
    let mut spec = JobSpec::new(
        id,
        Trigger::Every {
            interval: Duration::from_secs(86_400),
        },
        handler,
    );
    spec.persistent = true;
    spec
}

#[tokio::test]
async fn a_persistent_job_is_still_scheduled_after_a_restart() {
    let workspace = Workspace::new();
    let handler = Job::new(false);
    let harness = workspace
        .open(HarnessBuilder::new().component(JobComponent::new("jobs.test", handler.clone())))
        .await;

    let cx = harness.cx();
    harness
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .schedule(dormant("nightly", "jobs.test"), &cx)
        .await
        .expect("a persistent job is accepted by the durable scheduler");
    harness.stop().await;

    let restarted = workspace
        .open(HarnessBuilder::new().component(JobComponent::new("jobs.test", Job::new(false))))
        .await;
    let jobs = restarted
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .list(&cx)
        .await
        .expect("listing");
    assert_eq!(
        jobs.iter()
            .map(|job| job.spec.id.to_string())
            .collect::<Vec<_>>(),
        vec!["nightly".to_owned()],
        "the schedule did not survive the restart",
    );

    restarted.stop().await;
}

#[tokio::test]
async fn an_ephemeral_run_refuses_a_persistent_job_rather_than_forgetting_it() {
    // The one outcome that would let a deployment believe its nightly job exists.
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .ephemeral()
        .component(JobComponent::new("jobs.test", Job::new(false)))
        .build(workspace.root.path())
        .await;

    let error = harness
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .schedule(dormant("nightly", "jobs.test"), &harness.cx())
        .await
        .expect_err("a scheduler with no store cannot promise durability");
    assert_eq!(error.kind(), aik_core::ErrorKind::Unsupported, "{error}");

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// 6. the memory tools are reachable only through the registry, and are there
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_memory_tools_are_registered_and_offered_like_any_other() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .reply(Reply::answer("nothing to do")),
        )
        .await;

    let listed: Vec<String> = harness
        .tools()
        .list(&harness.cx())
        .await
        .expect("listing")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    for expected in ["memory.get", "memory.query", "memory.put"] {
        assert!(
            listed.contains(&expected.to_owned()),
            "{expected} missing from {listed:?}"
        );
    }
    assert!(
        !listed.contains(&"memory.delete".to_owned()),
        "deletion must not be registered unless it was asked for: {listed:?}",
    );

    // And the model is told about them by the ordinary path, not a special one.
    let mut session = Session::new(
        &harness.kernel.context(),
        &harness.settings,
        Console::new(&b""[..]),
        None,
    )
    .expect("a session");
    session.one_shot("hello".to_owned()).await.expect("a turn");
    let offered = harness.model.offered(0);
    assert!(offered.contains(&"memory.put".to_owned()), "{offered:?}");

    drop(session);
    harness.stop().await;
}

#[tokio::test]
async fn deletion_is_reachable_only_when_it_was_asked_for() {
    let workspace = Workspace::new();

    let harness = workspace
        .open(HarnessBuilder::new().policy(allow_all_memory()))
        .await;
    let error = harness
        .tools()
        .invoke(
            &ToolName::new("memory.delete"),
            json!({ "id": aik_api::memory::MemoryId::new().to_string() }),
            &harness.cx(),
        )
        .await
        .expect_err("the tool was never registered");
    assert_eq!(error.kind(), aik_core::ErrorKind::NotFound, "{error}");
    assert!(
        harness.settings.memory != MemorySet::Full,
        "the default must not be the one that can forget",
    );
    harness.stop().await;

    let full = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .memory(MemorySet::Full),
        )
        .await;
    let listed: Vec<String> = full
        .tools()
        .list(&full.cx())
        .await
        .expect("listing")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert!(listed.contains(&"memory.delete".to_owned()), "{listed:?}");
    full.stop().await;
}

#[tokio::test]
async fn no_memory_mode_removes_every_memory_tool_but_keeps_the_store() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .memory(MemorySet::Off),
        )
        .await;

    let listed: Vec<String> = harness
        .tools()
        .list(&harness.cx())
        .await
        .expect("listing")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert!(
        !listed.iter().any(|name| name.starts_with("memory.")),
        "a permissive policy must not put back a tool that was never registered: {listed:?}",
    );
    assert!(
        harness
            .kernel
            .context()
            .service::<dyn MemoryStore>()
            .is_ok(),
        "the store is infrastructure; only the door onto it was withheld",
    );

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// 7. policy is still the inner limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_registered_memory_tool_is_useless_without_a_policy() {
    let workspace = Workspace::new();
    let harness = workspace.open(HarnessBuilder::new()).await;

    let error = remember(&harness, "note", "should never be stored")
        .await
        .expect_err("with no policy configured, everything is denied");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");

    // And the denial has to be real, not merely reported: nothing may have reached the store.
    let store = harness
        .kernel
        .context()
        .service::<dyn MemoryStore>()
        .expect("a memory store");
    assert!(
        store
            .query(&MemoryQuery::default(), &harness.cx())
            .await
            .expect("querying the store directly")
            .is_empty(),
    );

    harness.stop().await;
}

#[tokio::test]
async fn policy_decides_which_kinds_a_memory_may_be_stored_under() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(HarnessBuilder::new().policy(allow_known_kinds()))
        .await;

    let allowed = remember(&harness, "note", "under an allowed kind")
        .await
        .expect("the allowed kind runs");
    assert!(!allowed.is_error, "{:?}", allowed.output);

    let error = remember(&harness, "invented", "under a kind nobody allowed")
        .await
        .expect_err("an unmatched question is a denial");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");

    assert_eq!(recall(&harness, "note").await.len(), 1);
    assert!(recall(&harness, "invented").await.is_empty());

    harness.stop().await;
}

/// The configuration this repository actually ships.
fn shipped_config() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("the example configuration is readable");
    serde_json::from_str(&text).expect("the example configuration is valid JSON")
}

#[tokio::test]
async fn the_shipped_example_configuration_describes_the_system_that_exists() {
    // The example is documentation people paste, so it has to keep working as the wiring
    // changes: an unknown settings key would fail the kernel, and a policy that named an
    // action no tool requires would look permissive while permitting nothing.
    let workspace = Workspace::new();
    let harness = HarnessBuilder::new()
        .config(shipped_config())
        .database(workspace.database())
        .build(workspace.root.path())
        .await;

    assert!(
        harness.settings.has_policy(),
        "the shipped configuration must carry a usable policy",
    );
    assert_eq!(
        harness
            .settings
            .system_prompt
            .as_deref()
            .map(str::trim)
            .map(|prompt| prompt.is_empty()),
        Some(false),
        "the shipped prompt is how the agent is told its memory exists",
    );

    for kind in ["preference", "fact", "note"] {
        let outcome = remember(&harness, kind, "something worth keeping")
            .await
            .unwrap_or_else(|error| panic!("`{kind}` should be allowed: {error}"));
        assert!(!outcome.is_error, "{:?}", outcome.output);
    }

    let error = remember(&harness, "arbitrary", "under a kind nobody allowed")
        .await
        .expect_err("the shipped policy allows only the kinds it names");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");

    // And what it says about deletion has to match what is registered: the rule exists so
    // that `--memory full` is still gated, not so that the default is.
    let listed: Vec<String> = harness
        .tools()
        .list(&harness.cx())
        .await
        .expect("listing")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert!(!listed.contains(&"memory.delete".to_owned()), "{listed:?}");

    harness.stop().await;
}

#[tokio::test]
async fn an_unrestricted_query_is_authorized_as_every_kind_at_once() {
    // A query naming no kind asks for all of them, so it claims `kind/*` rather than any
    // single kind. Which way that falls is entirely up to the rule's resource pattern, and
    // both directions are documented, so both are asserted.
    let workspace = Workspace::new();

    let permissive = workspace
        .open(HarnessBuilder::new().policy(allow_known_kinds()))
        .await;
    let outcome = permissive
        .tools()
        .invoke(&ToolName::new("memory.query"), json!({}), &permissive.cx())
        .await
        .expect("a `*` resource rule matches `kind/*` like any other resource");
    assert!(!outcome.is_error, "{:?}", outcome.output);
    permissive.stop().await;

    let scoped = workspace
        .open(HarnessBuilder::new().policy(json!([
            { "action": "memory.query", "effect": { "decision": "allow" } },
            { "action": "memory.query", "resource": "kind/note",
              "effect": { "decision": "allow" } }
        ])))
        .await;
    let error = scoped
        .tools()
        .invoke(&ToolName::new("memory.query"), json!({}), &scoped.cx())
        .await
        .expect_err("`kind/note` does not answer a question about `kind/*`");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");

    let outcome = scoped
        .tools()
        .invoke(
            &ToolName::new("memory.query"),
            json!({ "kinds": ["note"] }),
            &scoped.cx(),
        )
        .await
        .expect("naming the kind is what the rule asks for");
    assert!(!outcome.is_error, "{:?}", outcome.output);
    scoped.stop().await;
}

// ---------------------------------------------------------------------------
// 8. ownership and delegation, through the frontend's own principal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_memory_belongs_to_the_agent_that_stored_it() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .agent("assistant")
                .user("alice"),
        )
        .await;

    let outcome = remember(&harness, "note", "assistant's own")
        .await
        .expect("the tool runs");
    let id = outcome.output["id"].as_str().expect("an id").to_owned();
    harness.stop().await;

    // A different agent, over the same database, with a policy that allows everything: the
    // limit under test is ownership, not authorization.
    let other = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .agent("intruder")
                .user("bob"),
        )
        .await;
    let error = other
        .tools()
        .invoke(
            &ToolName::new("memory.get"),
            json!({ "id": id }),
            &other.cx(),
        )
        .await
        .expect_err("naming another principal's record is a denial");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");
    assert!(
        recall(&other, "note").await.is_empty(),
        "enumerating must not report a record that naming would refuse",
    );
    other.stop().await;
}

#[tokio::test]
async fn delegation_reaches_the_memories_of_the_principal_being_acted_for() {
    let workspace = Workspace::new();
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .agent("assistant")
                .user("alice"),
        )
        .await;
    let outcome = remember(&harness, "note", "assistant's own")
        .await
        .expect("the tool runs");
    let id = outcome.output["id"].as_str().expect("an id").to_owned();
    harness.stop().await;

    // `--agent helper --user assistant` is `Principal::new("helper", Agent)
    // .on_behalf_of("assistant")`, which is exactly the delegation `may_act_for` accepts.
    // That the frontend's two identity flags land in those two places is the whole point.
    let delegate = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .agent("helper")
                .user("assistant"),
        )
        .await;
    assert_eq!(
        delegate.settings.principal(),
        Principal::new("helper", PrincipalKind::Agent).on_behalf_of("assistant"),
    );

    let outcome = delegate
        .tools()
        .invoke(
            &ToolName::new("memory.get"),
            json!({ "id": id }),
            &delegate.cx(),
        )
        .await
        .expect("a delegate may act for the principal it acts for");
    assert!(!outcome.is_error, "{:?}", outcome.output);
    assert_eq!(
        outcome.output["record"]["content"],
        json!("assistant's own")
    );

    delegate.stop().await;
}

// ---------------------------------------------------------------------------
// 9. a firing and a turn against one database
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_job_and_an_agent_share_one_database_safely() {
    let workspace = Workspace::new();
    let handler = Job::new(true);
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .component(JobComponent::new("jobs.test", handler.clone())),
        )
        .await;

    let mut spec = JobSpec::new(
        "sweep",
        Trigger::Every {
            interval: Duration::from_millis(5),
        },
        "jobs.test",
    );
    spec.persistent = true;
    harness
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .schedule(spec, &harness.cx())
        .await
        .expect("scheduled");

    // The firings keep writing to the same redb file while the agent's own turns do. Both
    // sides have to keep succeeding: a write transaction one of them was starved of would
    // show up as an error here rather than as a wrong answer later.
    for turn in 0..10 {
        let outcome = remember(&harness, "note", &format!("turn {turn}"))
            .await
            .expect("the tool runs while jobs are firing");
        assert!(!outcome.is_error, "{:?}", outcome.output);
    }

    let deadline = std::time::Instant::now() + PATIENCE;
    while handler.firings() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(handler.firings() > 0, "the job never fired");

    // Each side sees only its own records: the firing runs as `scheduler` on behalf of the
    // job's owner, so what it stored is owned by `scheduler` and not by the agent.
    assert_eq!(recall(&harness, "note").await.len(), 10);
    assert!(
        recall(&harness, "job").await.is_empty(),
        "the agent must not inherit what a firing stored under its own identity",
    );

    let system = ExecutionContext::new().with_principal(Principal::new(
        aik_scheduler::RUN_PRINCIPAL,
        PrincipalKind::System,
    ));
    let theirs = harness
        .kernel
        .context()
        .service::<dyn MemoryStore>()
        .expect("a memory store")
        .query(
            &MemoryQuery {
                kinds: vec![aik_api::memory::MemoryKind::new("job")],
                ..MemoryQuery::default()
            },
            &system,
        )
        .await
        .expect("querying as the firing's own principal");
    assert!(!theirs.is_empty(), "the firing's writes were lost");

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// 10. shutdown lets go of everything
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_releases_the_database_so_the_next_run_can_open_it() {
    let workspace = Workspace::new();
    let handler = Job::new(false);
    let harness = workspace
        .open(
            HarnessBuilder::new()
                .policy(allow_all_memory())
                .component(JobComponent::new("jobs.test", handler.clone())),
        )
        .await;

    remember(&harness, "note", "before shutdown")
        .await
        .expect("stored");
    harness
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .schedule(dormant("nightly", "jobs.test"), &harness.cx())
        .await
        .expect("scheduled");

    // redb's lock is the assertion. If shutdown left a background sweep, a scheduler
    // driver, a tool binding or the registry holding an `Arc<Db>`, this open fails.
    harness.stop().await;
    let db = Db::open(workspace.database()).expect("the lock was released");
    drop(db);

    // And a whole frontend can be started over it again, which is what a restart is.
    let restarted = workspace
        .open(HarnessBuilder::new().policy(allow_all_memory()))
        .await;
    assert_eq!(recall(&restarted, "note").await.len(), 1);
    restarted.stop().await;
}

#[tokio::test]
async fn shutdown_stops_the_scheduler_rather_than_leaving_it_running() {
    let workspace = Workspace::new();
    let handler = Job::new(false);
    let harness = workspace
        .open(HarnessBuilder::new().component(JobComponent::new("jobs.test", handler.clone())))
        .await;

    let mut spec = JobSpec::new(
        "fast",
        Trigger::Every {
            interval: Duration::from_millis(5),
        },
        "jobs.test",
    );
    spec.persistent = true;
    harness
        .kernel
        .context()
        .service::<dyn Scheduler>()
        .expect("a scheduler")
        .schedule(spec, &harness.cx())
        .await
        .expect("scheduled");

    let deadline = std::time::Instant::now() + PATIENCE;
    while handler.firings() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(handler.firings() > 0, "the job never fired");

    harness.stop().await;
    let after = handler.firings();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        handler.firings(),
        after,
        "a firing after shutdown means the driver outlived the kernel",
    );
}
