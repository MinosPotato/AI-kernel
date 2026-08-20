//! Fixtures shared by the context test suites.
//!
//! Every behavioural assertion about a [`ContextStore`] is written once and run against
//! both implementations, because the persistent store's job is to be
//! indistinguishable from the in-memory one except for surviving a restart. A test that
//! only ever ran against one of them would let the two drift, and the drift would show up
//! as a transcript that reads differently after a restart — the least testable moment
//! there is.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aik_api::context::{ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, Message, Role};
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::tool::{ToolCall, ToolName};
use aik_context::{DEFAULT_MAX_RECORDS_PER_SESSION, InMemoryContextStore, RedbContextStore};
use aik_store::Db;
use serde_json::json;
use tempfile::TempDir;

/// Which implementation a suite function is being run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// [`InMemoryContextStore`].
    Memory,
    /// [`RedbContextStore`], over a database in a temporary directory.
    Redb,
}

impl Backend {
    /// Opens a store with the default per-session record bound.
    pub(crate) fn open(self) -> Fixture {
        self.bounded(DEFAULT_MAX_RECORDS_PER_SESSION)
    }

    /// Opens a store that refuses more than `max_records` records in one session.
    pub(crate) fn bounded(self, max_records: usize) -> Fixture {
        match self {
            Self::Memory => Fixture {
                store: Some(Arc::new(
                    InMemoryContextStore::new().with_max_records(max_records),
                )),
                directory: None,
                path: None,
                max_records,
            },
            Self::Redb => {
                let directory = tempfile::tempdir().expect("a temporary directory");
                let path = directory.path().join("aik.redb");
                Fixture {
                    store: Some(open_redb(&path, max_records)),
                    directory: Some(directory),
                    path: Some(path),
                    max_records,
                }
            }
        }
    }
}

/// A store, plus whatever has to stay alive for it to keep working.
pub(crate) struct Fixture {
    store: Option<Arc<dyn ContextStore>>,
    directory: Option<TempDir>,
    path: Option<PathBuf>,
    max_records: usize,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fixture").field("path", &self.path).finish()
    }
}

impl Fixture {
    /// The store under test.
    pub(crate) fn store(&self) -> Arc<dyn ContextStore> {
        self.store.clone().expect("the fixture holds a store")
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
        self.store = Some(open_redb(&path, self.max_records));
    }

    /// Drops the store, releasing redb's exclusive lock on the file.
    ///
    /// What a test wants before opening the same database by hand. The temporary directory
    /// stays alive, so the file is still there.
    pub(crate) fn close(&mut self) {
        self.store = None;
    }
}

/// Opens a persistent store over the database at `path`.
pub(crate) fn open_redb(path: &Path, max_records: usize) -> Arc<dyn ContextStore> {
    let db = Arc::new(Db::open(path).expect("the database opens"));
    Arc::new(
        RedbContextStore::new(db)
            .expect("the context tables are created")
            .with_max_records(max_records),
    )
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

/// An ordinary user turn.
pub(crate) fn say(body: &str) -> ContextEntry {
    ContextEntry::new(Message::text(Role::User, body))
}

/// One assistant turn that calls a tool, and the tool result that answers it.
pub(crate) fn tool_exchange(call_id: &str, payload: &str) -> (ContextEntry, ContextEntry) {
    let call = ContextEntry::new(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(ToolCall {
            call_id: call_id.into(),
            name: ToolName::new("filesystem.read"),
            arguments: json!({ "path": "src/lib.rs" }),
        })],
        name: None,
    });
    let result = ContextEntry::new(Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult {
            call_id: call_id.into(),
            content: json!({ "path": "src/lib.rs", "content": payload }),
            is_error: false,
        }],
        name: None,
    });
    (call, result)
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
