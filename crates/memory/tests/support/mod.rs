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
