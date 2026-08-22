//! Fixtures shared by the audit test suites.
//!
//! Every behavioural assertion about an [`AuditStore`] is written once and run against both
//! implementations, because the persistent store's job is to be indistinguishable from the
//! in-memory one except for surviving a restart. A test that only ever ran against one of them
//! would let the two drift, and the two are the same guarantee.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aik_api::audit::{
    AuditEntry, AuditStore, AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase,
    InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, Principal, PrincipalId, PrincipalKind, ResourceId};
use aik_api::tool::ToolName;
use aik_audit::{AuditRetentionSweeper, InMemoryAuditStore, RedbAuditStore};
use aik_core::clock::{ManualClock, Timestamp};
use aik_core::id::CorrelationId;
use aik_store::Db;
use tempfile::TempDir;

/// Which implementation a suite function is being run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// [`InMemoryAuditStore`].
    Memory,
    /// [`RedbAuditStore`], over a database in a temporary directory.
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
                let concrete = Arc::new(InMemoryAuditStore::new().with_clock(clock.clone()));
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
    store: Option<Arc<dyn AuditStore>>,
    sweeper: Option<Arc<dyn AuditRetentionSweeper>>,
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
    pub(crate) fn store(&self) -> Arc<dyn AuditStore> {
        self.store.clone().expect("the fixture holds a store")
    }

    /// The clock backing the store, for stamping retention markers deterministically.
    pub(crate) fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// The sweeper, the one capability that can remove anything.
    pub(crate) fn sweeper(&self) -> Arc<dyn AuditRetentionSweeper> {
        self.sweeper.clone().expect("the fixture holds a sweeper")
    }

    /// Sweeps everything at or before `cutoff`, returning how many records went.
    pub(crate) async fn sweep(&self, cutoff: Timestamp) -> usize {
        self.sweeper()
            .sweep_older_than(cutoff)
            .await
            .expect("sweeping should not fail against a healthy database")
    }

    /// Where the database lives, for a persistent fixture.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("a persistent fixture")
    }

    /// Closes the store and opens a new one over the same file. This is what a restart is.
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
) -> (Arc<dyn AuditStore>, Arc<dyn AuditRetentionSweeper>) {
    let db = Arc::new(Db::open(path).expect("the database opens"));
    let concrete = Arc::new(
        RedbAuditStore::new(db)
            .expect("the audit tables are created")
            .with_clock(clock),
    );
    (concrete.clone(), concrete)
}

/// A context reading as a named user.
pub(crate) fn user(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
}

/// A context reading as an agent working for `owner`.
pub(crate) fn agent_for(id: &str, owner: &str) -> ExecutionContext {
    ExecutionContext::new()
        .with_principal(Principal::new(id, PrincipalKind::Agent).on_behalf_of(owner))
}

/// A context naming no principal at all, which is the system acting for itself.
pub(crate) fn anonymous() -> ExecutionContext {
    ExecutionContext::new()
}

// --- entry builders -------------------------------------------------------------------

/// An allowed tool-level authorization decision by `principal`, at `at`.
pub(crate) fn allowed(principal: &str, tool: &str, at: u64) -> AuditEntry {
    decision(
        principal,
        None,
        tool,
        at,
        CorrelationId::new(),
        AuthorizationOutcome::Allowed,
    )
}

/// A denied resource-level decision by `principal`, at `at`.
pub(crate) fn denied(principal: &str, tool: &str, at: u64) -> AuditEntry {
    decision(
        principal,
        None,
        tool,
        at,
        CorrelationId::new(),
        AuthorizationOutcome::Denied {
            reason: "outside the workspace".into(),
        },
    )
}

/// A decision with every field under the caller's control.
pub(crate) fn decision(
    principal: &str,
    on_behalf_of: Option<&str>,
    tool: &str,
    at: u64,
    correlation: CorrelationId,
    outcome: AuthorizationOutcome,
) -> AuditEntry {
    AuditEntry::Authorization(AuthorizationDecided {
        correlation,
        timestamp: Timestamp::from_millis(at),
        tool: ToolName::new(tool),
        principal: PrincipalId::new(principal),
        principal_kind: PrincipalKind::Agent,
        on_behalf_of: on_behalf_of.map(PrincipalId::new),
        action: ActionId::new("fs.read"),
        resource: Some(ResourceId::new("/tmp/notes.txt")),
        phase: AuthorizationPhase::Resource,
        duration_ms: 1,
        approval_wait_ms: None,
        outcome,
    })
}

/// A successful invocation by `principal`, at `at`.
pub(crate) fn invoked(principal: &str, tool: &str, at: u64) -> AuditEntry {
    invocation(
        principal,
        None,
        tool,
        at,
        CorrelationId::new(),
        InvocationOutcome::Succeeded,
    )
}

/// An invocation with every field under the caller's control.
pub(crate) fn invocation(
    principal: &str,
    on_behalf_of: Option<&str>,
    tool: &str,
    at: u64,
    correlation: CorrelationId,
    outcome: InvocationOutcome,
) -> AuditEntry {
    AuditEntry::Invocation(ToolInvoked {
        correlation,
        timestamp: Timestamp::from_millis(at),
        tool: ToolName::new(tool),
        principal: PrincipalId::new(principal),
        principal_kind: PrincipalKind::Agent,
        on_behalf_of: on_behalf_of.map(PrincipalId::new),
        duration_ms: 2,
        authorization_duration_ms: Some(1),
        execution_duration_ms: Some(1),
        outcome,
    })
}

/// Runs one suite function against both implementations.
///
/// Each name becomes a module with an `in_memory` and a `redb` test in it, so a failure names
/// the assertion *and* the backend that broke it.
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
