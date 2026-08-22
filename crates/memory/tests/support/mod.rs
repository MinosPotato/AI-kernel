//! Fixtures shared by the memory test suites.
//!
//! Every behavioural assertion about a [`MemoryStore`] is written once and run against both
//! implementations, because the persistent store's job is to be indistinguishable from the
//! in-memory one except for surviving a restart and for reclaiming space in the background.
//! A test that only ever ran against one of them would let the two drift.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::memory::MemoryStore;
use aik_api::permission::{Principal, PrincipalKind};
use aik_core::Clock;
use aik_core::clock::{ManualClock, Timestamp};
use aik_memory::{ExpirySweeper, InMemoryMemoryStore, RedbMemoryStore};
use aik_store::Db;
use tempfile::TempDir;

/// Which implementation a suite function is being run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// [`InMemoryMemoryStore`].
    Memory,
    /// [`RedbMemoryStore`], over a database in a temporary directory.
    Redb,
}

impl Backend {
    /// Opens a store with a manual clock stopped at the epoch.
    pub(crate) fn open(self) -> Fixture {
        self.at(Timestamp::EPOCH)
    }

    /// Opens a store with a manual clock stopped at `start`.
    pub(crate) fn at(self, start: Timestamp) -> Fixture {
        let clock = Arc::new(ManualClock::new(start));
        match self {
            Self::Memory => {
                let concrete = Arc::new(InMemoryMemoryStore::new().with_clock(clock.clone()));
                Fixture {
                    store: Some(concrete.clone()),
                    sweeper: Some(concrete),
                    clock,
                    directory: None,
                    path: None,
                }
            }
            Self::Redb => {
                let directory = tempfile::tempdir().expect("a temporary directory");
                let path = directory.path().join("aik.redb");
                let (store, sweeper) = open_redb(&path, clock.clone());
                Fixture {
                    store: Some(store),
                    sweeper: Some(sweeper),
                    clock,
                    directory: Some(directory),
                    path: Some(path),
                }
            }
        }
    }
}

/// A store, plus whatever has to stay alive for it to keep working.
pub(crate) struct Fixture {
    store: Option<Arc<dyn MemoryStore>>,
    sweeper: Option<Arc<dyn ExpirySweeper>>,
    clock: Arc<ManualClock>,
    directory: Option<TempDir>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fixture").field("path", &self.path).finish()
    }
}

impl Fixture {
    /// The store under test.
    pub(crate) fn store(&self) -> Arc<dyn MemoryStore> {
        self.store.clone().expect("the fixture holds a store")
    }

    /// The clock backing the store, for advancing time deterministically.
    pub(crate) fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// Runs an expiry sweep at the clock's current time and returns how many records it
    /// removed.
    pub(crate) async fn sweep(&self) -> usize {
        self.sweeper()
            .sweep_expired(self.clock.now())
            .await
            .expect("sweeping should not fail against a healthy database")
    }

    /// The sweeper itself, for a test that needs to see a sweep *fail* rather than assume it
    /// succeeds.
    pub(crate) fn sweeper(&self) -> Arc<dyn ExpirySweeper> {
        self.sweeper.clone().expect("the fixture holds a sweeper")
    }

    /// Where the database lives, for a persistent fixture.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("a persistent fixture")
    }

    /// Closes the store and opens a new one over the same file.
    ///
    /// This is what a restart is. The old handle is dropped first and deliberately: redb
    /// holds an exclusive lock on the file, so a reopen that succeeds is also proof that the
    /// previous one released everything it held.
    pub(crate) fn reopen(&mut self) {
        let path = self
            .path
            .clone()
            .expect("only a persistent fixture can be reopened");
        self.store = None;
        self.sweeper = None;
        let (store, sweeper) = open_redb(&path, self.clock.clone());
        self.store = Some(store);
        self.sweeper = Some(sweeper);
    }

    /// Drops the store, releasing redb's exclusive lock on the file.
    pub(crate) fn close(&mut self) {
        self.store = None;
        self.sweeper = None;
    }
}

/// Opens a persistent store over the database at `path`, as both of its capabilities.
pub(crate) fn open_redb(
    path: &Path,
    clock: Arc<ManualClock>,
) -> (Arc<dyn MemoryStore>, Arc<dyn ExpirySweeper>) {
    let db = Arc::new(Db::open(path).expect("the database opens"));
    let concrete = Arc::new(
        RedbMemoryStore::new(db)
            .expect("the memory tables are created")
            .with_clock(clock),
    );
    (concrete.clone(), concrete)
}

/// A context acting as a named user.
pub(crate) fn user(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
}

/// A context acting as an agent working for `owner`.
pub(crate) fn agent_for(id: &str, owner: &str) -> ExecutionContext {
    ExecutionContext::new()
        .with_principal(Principal::new(id, PrincipalKind::Agent).on_behalf_of(owner))
}

/// A context naming no principal at all, which is the system acting for itself.
pub(crate) fn anonymous() -> ExecutionContext {
    ExecutionContext::new()
}

/// Runs one suite function against both implementations.
///
/// Each name becomes a module with an `in_memory` and a `redb` test in it, so a failure
/// names the assertion *and* the backend that broke it.
#[macro_export]
macro_rules! both_backends {
    ($($name:ident),+ $(,)?) => {
        $(
            mod $name {
                #[tokio::test]
                async fn in_memory() {
                    super::$name($crate::support::Backend::Memory).await;
                }

                #[tokio::test]
                async fn redb() {
                    super::$name($crate::support::Backend::Redb).await;
                }
            }
        )+
    };
}

// --- the memory tools -----------------------------------------------------------------

/// A running kernel with a memory store, a bound set of memory tools, and — for the durable
/// backend — a database file that survives a restart.
///
/// The tools are bound the way a real deployment binds them: by
/// [`MemoryToolsComponent`](aik_memory::MemoryToolsComponent) during kernel `init`, against
/// whichever [`MemoryStore`] the kernel published. That is the whole reason this fixture
/// builds a kernel rather than handing a store to a tool directly — wiring that only ever
/// happened in a test would prove nothing about the wiring that ships.
pub(crate) struct ToolKernel {
    kernel: Option<aik_core::Kernel>,
    tools: Option<Tools>,
    clock: Arc<ManualClock>,
    backend: Backend,
    directory: Option<TempDir>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for ToolKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolKernel")
            .field("backend", &self.backend)
            .field("path", &self.path)
            .finish()
    }
}

/// The four tools one [`MemoryToolsComponent`](aik_memory::MemoryToolsComponent) handed out.
#[derive(Debug)]
pub(crate) struct Tools {
    pub(crate) put: aik_memory::MemoryPutTool,
    pub(crate) get: aik_memory::MemoryGetTool,
    pub(crate) query: aik_memory::MemoryQueryTool,
    pub(crate) delete: aik_memory::MemoryDeleteTool,
}

impl Backend {
    /// Starts a kernel with this backend and a manual clock stopped at the epoch.
    pub(crate) async fn open_tools(self) -> ToolKernel {
        let clock = Arc::new(ManualClock::new(Timestamp::EPOCH));
        let (directory, path) = match self {
            Self::Memory => (None, None),
            Self::Redb => {
                let directory = tempfile::tempdir().expect("a temporary directory");
                let path = directory.path().join("aik.redb");
                (Some(directory), Some(path))
            }
        };
        let (kernel, tools) = start_tool_kernel(self, clock.clone(), path.as_deref()).await;
        ToolKernel {
            kernel: Some(kernel),
            tools: Some(tools),
            clock,
            backend: self,
            directory,
            path,
        }
    }
}

impl ToolKernel {
    /// The tools under test.
    pub(crate) fn tools(&self) -> &Tools {
        self.tools.as_ref().expect("the kernel is running")
    }

    /// The clock the kernel stamps records with.
    pub(crate) fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// The store the tools are bound to, for asserting on what they actually wrote.
    pub(crate) fn store(&self) -> Arc<dyn MemoryStore> {
        self.kernel
            .as_ref()
            .expect("the kernel is running")
            .context()
            .service::<dyn MemoryStore>()
            .expect("the memory store is published")
    }

    /// Shuts the kernel down and starts a new one over the same database.
    ///
    /// The old kernel is stopped first and deliberately: redb holds an exclusive lock on the
    /// file, so a restart that succeeds is also proof the previous one released everything.
    /// The tools are rebuilt too, because a restart hands out a new binding — a tool that
    /// kept working across a restart would be a tool holding a store the kernel no longer
    /// owns.
    pub(crate) async fn restart(&mut self) {
        let kernel = self.kernel.take().expect("the kernel is running");
        kernel.shutdown().await.expect("the kernel stops cleanly");
        // Dropping both is part of stopping here, and in this order. The database is held by
        // the registry the kernel owns *and* by the binding behind every tool it handed out;
        // redb refuses to open one file twice, so a restart that kept either alive would fail
        // to open the very file it is restarting on.
        drop(kernel);
        drop(self.tools.take());
        let (kernel, tools) =
            start_tool_kernel(self.backend, self.clock.clone(), self.path.as_deref()).await;
        self.kernel = Some(kernel);
        self.tools = Some(tools);
    }

    /// Stops the kernel, failing the test if it does not stop cleanly.
    pub(crate) async fn shutdown(mut self) {
        if let Some(kernel) = self.kernel.take() {
            kernel.shutdown().await.expect("the kernel stops cleanly");
        }
        drop(self.tools.take());
        drop(self.directory.take());
    }
}

/// Builds and starts one kernel wired for `backend`, returning it with its memory tools.
async fn start_tool_kernel(
    backend: Backend,
    clock: Arc<ManualClock>,
    path: Option<&Path>,
) -> (aik_core::Kernel, Tools) {
    use aik_core::prelude::*;

    let component = aik_memory::MemoryToolsComponent::new();
    let tools = Tools {
        put: component.put(),
        get: component.get(),
        query: component.query(),
        delete: component.delete(),
    };

    let builder = Kernel::builder().clock(clock);
    let builder = match backend {
        Backend::Memory => builder.component(aik_memory::MemoryComponent::new()),
        Backend::Redb => builder
            .config(store_config(path.expect("a persistent backend has a path")))
            .component(aik_store::StoreComponent::new())
            .component(aik_memory::RedbMemoryComponent::new()),
    };
    let kernel = builder
        .component(component)
        .build()
        .expect("a valid kernel");
    kernel.start().await.expect("the kernel starts");
    (kernel, tools)
}

/// Configuration pointing the shared database at `path`.
pub(crate) fn store_config(path: &Path) -> aik_core::Config {
    aik_core::Config::builder()
        .layer(serde_json::json!({ "components": { "store": { "db": { "path": path } } } }))
        .build()
}

/// Stands in for the authorizer a [`ToolRegistry`](aik_api::tool::ToolRegistry) supplies.
///
/// These suites call [`Tool::invoke`](aik_api::tool::Tool::invoke) directly, because what
/// they are about is what the tools do with a store. That the registry is the only door to
/// them, and that policy is consulted at it, is asserted against a real registry in the
/// cross-subsystem suite — this crate deliberately does not depend on `aik-tools`.
#[derive(Debug)]
pub(crate) struct NoDiscoveredResources;

#[async_trait::async_trait]
impl aik_api::permission::ResourceAuthorizer for NoDiscoveredResources {
    async fn authorize(
        &self,
        _action: &aik_api::permission::ActionId,
        _resource: &aik_api::permission::ResourceId,
    ) -> aik_core::Result<()> {
        unreachable!("the memory tools declare every resource they touch in advance")
    }
}

/// Invokes a tool the way a registry would, minus the authorization it would have done.
pub(crate) async fn invoke(
    tool: &impl aik_api::tool::Tool,
    arguments: serde_json::Value,
    cx: &ExecutionContext,
) -> aik_core::Result<aik_api::tool::ToolOutcome> {
    tool.invoke(arguments, &NoDiscoveredResources, cx).await
}

/// Invokes a tool and unwraps a successful, non-error outcome's output.
pub(crate) async fn output(
    tool: &impl aik_api::tool::Tool,
    arguments: serde_json::Value,
    cx: &ExecutionContext,
) -> serde_json::Value {
    let outcome = invoke(tool, arguments, cx)
        .await
        .expect("the tool call should succeed");
    assert!(!outcome.is_error, "unexpected error outcome: {outcome:?}");
    outcome.output
}
