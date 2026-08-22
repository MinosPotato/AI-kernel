//! Fixtures shared by the cross-subsystem suites.
//!
//! Everything here assembles *real* kernels. There are no stubs for the subsystems under
//! test: the point of this crate is that the seams between them behave, and a seam tested
//! against a double is a seam nobody has tested.

#![allow(dead_code, unreachable_pub)]

pub mod agent;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::scheduler::{JobHandler, JobSpec};
use aik_core::prelude::*;
use aik_core::{Config, Result};
use serde_json::json;

/// How long a test waits for something it expects to arrive before failing rather than
/// hanging. Generous, because only a broken assertion ever waits this long.
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

/// A context acting as a named user.
pub fn user(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
}

/// A context naming nobody, which is the system acting for itself.
pub fn anonymous() -> ExecutionContext {
    ExecutionContext::new()
}

/// Configuration pointing the shared store at `path`.
pub fn store_config(path: &Path) -> Config {
    Config::builder()
        .layer(json!({ "components": { "store": { "db": { "path": path } } } }))
        .build()
}

/// Yields until `condition` holds, failing the test rather than hanging if it never does.
pub async fn until(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + PATIENCE;
    while std::time::Instant::now() < deadline {
        if condition().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("gave up waiting for {what}");
}

/// What a [`RecordingHandler`] saw when it was called.
#[derive(Debug, Clone)]
pub struct Firing {
    /// Which job fired.
    pub job: String,
    /// The principal the firing carried, which is what every downstream check keys on.
    pub principal: Option<Principal>,
}

/// A [`JobHandler`] that records how it was called and optionally runs a closure.
///
/// One handler type for every suite, because what differs between them is what the job
/// *does*, not what needs recording about it.
pub struct RecordingHandler {
    firings: std::sync::Mutex<Vec<Firing>>,
    body: Option<Body>,
    hold: AtomicBool,
    noticed_cancellation: AtomicBool,
    calls: AtomicUsize,
}

/// A boxed future, so a suite can hand the handler an arbitrary async body without this
/// crate taking a futures dependency for one type alias.
pub type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;

type Body = Box<dyn Fn(JobSpec, ExecutionContext) -> BoxFuture + Send + Sync>;

impl std::fmt::Debug for RecordingHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingHandler")
            .field("calls", &self.calls.load(Ordering::SeqCst))
            .finish()
    }
}

impl Default for RecordingHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingHandler {
    /// Records the call and returns successfully.
    pub fn new() -> Self {
        Self {
            firings: std::sync::Mutex::new(Vec::new()),
            body: None,
            hold: AtomicBool::new(false),
            noticed_cancellation: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        }
    }

    /// Records the call, then runs `body` with the firing's own spec and context.
    pub fn running<F>(body: F) -> Self
    where
        F: Fn(JobSpec, ExecutionContext) -> BoxFuture + Send + Sync + 'static,
    {
        Self {
            body: Some(Box::new(body)),
            ..Self::new()
        }
    }

    /// Records the call, then blocks until its context is cancelled and reports that it
    /// stopped because of it.
    ///
    /// Returning [`Error::Cancelled`] rather than `Ok` is the part that matters. The
    /// scheduler cannot see *why* a handler returned, so a handler that notices cancellation
    /// and returns `Ok` has, as far as the scheduler can tell, finished its work — and is
    /// published as [`JobCompleted`](aik_api::scheduler::JobCompleted). A handler that
    /// abandoned its work has to say so.
    pub fn holding() -> Self {
        Self {
            hold: AtomicBool::new(true),
            ..Self::new()
        }
    }

    /// Every firing so far.
    pub fn firings(&self) -> Vec<Firing> {
        self.firings.lock().expect("no test panics here").clone()
    }

    /// How many times the handler was called.
    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Whether a firing ever observed its context being cancelled.
    pub fn noticed_cancellation(&self) -> bool {
        self.noticed_cancellation.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl JobHandler for RecordingHandler {
    async fn run(&self, job: &JobSpec, cx: &ExecutionContext) -> Result<()> {
        self.firings
            .lock()
            .expect("no test panics here")
            .push(Firing {
                job: job.id.to_string(),
                principal: cx.principal.clone(),
            });
        self.calls.fetch_add(1, Ordering::SeqCst);

        if self.hold.load(Ordering::SeqCst) {
            cx.cancelled().await;
            self.noticed_cancellation.store(true, Ordering::SeqCst);
            return Err(aik_core::Error::Cancelled);
        }
        match &self.body {
            Some(body) => body(job.clone(), cx.clone()).await,
            None => Ok(()),
        }
    }
}

/// Publishes one [`JobHandler`] under a component id a job can name, the way any subsystem
/// contributing scheduled work would.
pub struct HandlerComponent {
    id: ComponentId,
    handler: Arc<RecordingHandler>,
    requires: Vec<ComponentId>,
}

impl std::fmt::Debug for HandlerComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerComponent")
            .field("id", &self.id)
            .finish()
    }
}

impl HandlerComponent {
    /// Publishes `handler` under `id`.
    pub fn new(id: &str, handler: Arc<RecordingHandler>) -> Self {
        Self {
            id: ComponentId::new(id),
            handler,
            requires: Vec::new(),
        }
    }

    /// Declares a dependency, for a handler that resolves another subsystem in `init`.
    #[must_use]
    pub fn requiring(mut self, id: &str) -> Self {
        self.requires.push(ComponentId::new(id));
        self
    }
}

#[async_trait]
impl Component for HandlerComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        self.requires
            .iter()
            .fold(ComponentDescriptor::new(self.id.clone()), |d, id| {
                d.requires(id.clone())
            })
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide::<dyn JobHandler>(self.handler.clone())
    }
}

/// Runs one suite function against both memory implementations.
///
/// Each name becomes a module with an `in_memory` and a `redb` test in it, so a failure names
/// the assertion *and* the backend that broke it. The durable store's job is to be
/// indistinguishable from the volatile one except for surviving a restart, and an agent that
/// could reach further into one of them than the other would be exactly the kind of
/// difference that claim is supposed to exclude.
#[macro_export]
macro_rules! both_backends {
    ($($name:ident),+ $(,)?) => {
        $(
            mod $name {
                #[tokio::test]
                async fn in_memory() {
                    super::$name($crate::support::agent::Backend::Memory).await;
                }

                #[tokio::test]
                async fn redb() {
                    super::$name($crate::support::agent::Backend::Redb).await;
                }
            }
        )+
    };
}
