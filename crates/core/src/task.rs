//! Structured concurrency for background work.
//!
//! Long-running background work — watching a compositor socket, draining a queue, running
//! an autonomous agent — must be cancellable and must be waited for at shutdown. [`Tasks`]
//! pairs a hierarchical [`CancellationToken`] with a tracker, so that:
//!
//! * cancelling a scope cancels every task spawned in it **and** in its child scopes;
//! * cancelling a child scope leaves siblings and the parent running;
//! * shutdown can wait for everything to actually finish, with a deadline.
//!
//! Every component gets its own child scope, so one component can be stopped without
//! disturbing the rest, while kernel shutdown still reaches all of them.
//!
//! ```
//! # use std::sync::Arc;
//! # use std::sync::atomic::{AtomicBool, Ordering};
//! # use std::time::Duration;
//! # use aik_core::task::Tasks;
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let tasks = Tasks::new();
//! let stopped = Arc::new(AtomicBool::new(false));
//!
//! tasks.spawn("heartbeat", {
//!     let stopped = stopped.clone();
//!     let token = tasks.cancellation_token();
//!     async move {
//!         token.cancelled().await;
//!         stopped.store(true, Ordering::SeqCst);
//!     }
//! });
//!
//! tasks.shutdown(Duration::from_secs(1)).await.unwrap();
//! assert!(stopped.load(Ordering::SeqCst));
//! # }
//! ```

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::error::{Error, Result};
use crate::id::TaskId;

/// A handle to one spawned task.
///
/// Dropping the handle detaches the task; it keeps running and is still waited for at
/// shutdown.
#[derive(Debug)]
pub struct TaskHandle<T> {
    id: TaskId,
    name: String,
    handle: JoinHandle<T>,
}

impl<T> TaskHandle<T> {
    /// The task's generated identifier.
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// The name the task was spawned with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Requests that the task be aborted at its next await point.
    ///
    /// Prefer cancelling the scope: abort gives the task no chance to clean up.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Returns true once the task has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Waits for the task to finish and returns its output.
    pub async fn join(self) -> Result<T> {
        match self.handle.await {
            Ok(value) => Ok(value),
            Err(error) if error.is_cancelled() => Err(Error::Cancelled),
            Err(error) => Err(Error::wrap(format!("task `{}` panicked", self.name), error)),
        }
    }
}

/// A cancellation scope with completion tracking.
///
/// Cloning yields a handle to the *same* scope. Use [`Tasks::child`] for a nested scope.
#[derive(Debug, Clone)]
pub struct Tasks {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl Default for Tasks {
    fn default() -> Self {
        Self::new()
    }
}

impl Tasks {
    /// Creates a root scope.
    pub fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            token: CancellationToken::new(),
        }
    }

    /// Creates a nested scope.
    ///
    /// The child can be cancelled on its own; cancelling the parent also cancels the
    /// child. Completion is tracked by the same tracker, so the root scope's shutdown
    /// waits for tasks spawned in any descendant.
    pub fn child(&self) -> Self {
        Self {
            tracker: self.tracker.clone(),
            token: self.token.child_token(),
        }
    }

    /// Returns this scope's cancellation token.
    ///
    /// Pass it into a task and select on [`CancellationToken::cancelled`] to shut down
    /// cooperatively.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Returns true once this scope has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Cancels this scope and all of its descendants, without waiting.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Spawns a tracked task.
    ///
    /// The task is *not* cancelled automatically — it must observe the token itself. Use
    /// [`Tasks::spawn_cancellable`] to have the token handed to you, or
    /// [`Tasks::spawn_until_cancelled`] to have cancellation handled for you.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    pub fn spawn<F>(&self, name: impl Into<String>, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let id = TaskId::new();
        let name = name.into();
        tracing::trace!(task = %name, %id, "spawning task");
        let handle = self.tracker.spawn(future);
        TaskHandle { id, name, handle }
    }

    /// Spawns a tracked task, handing it this scope's cancellation token.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    pub fn spawn_cancellable<F, Fut>(
        &self,
        name: impl Into<String>,
        f: F,
    ) -> TaskHandle<Fut::Output>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn(name, f(self.cancellation_token()))
    }

    /// Spawns a tracked task that is dropped as soon as the scope is cancelled.
    ///
    /// The output is `None` if cancellation won the race. This suits work that has nothing
    /// to clean up; anything holding resources should take the token and unwind properly.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    pub fn spawn_until_cancelled<F>(
        &self,
        name: impl Into<String>,
        future: F,
    ) -> TaskHandle<Option<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let token = self.cancellation_token();
        self.spawn(name, async move { token.run_until_cancelled(future).await })
    }

    /// Returns how many tracked tasks are still running.
    pub fn running(&self) -> usize {
        self.tracker.len()
    }

    /// Cancels the scope and waits for its tasks to finish.
    ///
    /// Returns [`Error::Timeout`] if they do not finish in time; the tasks are left
    /// running, since forcibly aborting them could leave shared state inconsistent. The
    /// caller decides what to do about it.
    ///
    /// Because the tracker is shared by every scope in the tree, this waits for tasks
    /// spawned in descendant scopes too, and should be called by whoever owns the root
    /// scope. A component that only wants to stop its own work should call
    /// [`Tasks::cancel`] on its scope instead.
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.token.cancel();
        self.tracker.close();
        match tokio::time::timeout(timeout, self.tracker.wait()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::Timeout(timeout)),
        }
    }

    /// Waits until this scope is cancelled.
    ///
    /// This is how a frontend blocks until something asks the system to stop.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn shutdown_cancels_and_waits() {
        let tasks = Tasks::new();
        let finished = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let finished = finished.clone();
            tasks.spawn_cancellable("worker", move |token| async move {
                token.cancelled().await;
                finished.fetch_add(1, Ordering::SeqCst);
            });
        }

        tasks.shutdown(Duration::from_secs(5)).await.unwrap();
        assert_eq!(finished.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cancelling_a_child_leaves_the_parent_alone() {
        let parent = Tasks::new();
        let child = parent.child();

        child.cancel();

        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn cancelling_the_parent_reaches_children() {
        let parent = Tasks::new();
        let child = parent.child();
        let grandchild = child.child();

        parent.cancel();

        assert!(child.is_cancelled());
        assert!(grandchild.is_cancelled());
    }

    #[tokio::test]
    async fn the_root_waits_for_tasks_spawned_in_children() {
        let root = Tasks::new();
        let child = root.child();
        let done = Arc::new(AtomicUsize::new(0));

        child.spawn_cancellable("child-worker", {
            let done = done.clone();
            move |token| async move {
                token.cancelled().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
                done.fetch_add(1, Ordering::SeqCst);
            }
        });

        root.shutdown(Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_reports_tasks_that_refuse_to_stop() {
        let tasks = Tasks::new();
        tasks.spawn("stubborn", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let error = tasks.shutdown(Duration::from_millis(20)).await.unwrap_err();
        assert!(matches!(error, Error::Timeout(_)), "{error}");
    }

    #[tokio::test]
    async fn spawn_until_cancelled_reports_which_side_won() {
        let tasks = Tasks::new();

        let completed = tasks.spawn_until_cancelled("quick", async { 42 });
        assert_eq!(completed.join().await.unwrap(), Some(42));

        let interrupted = tasks.spawn_until_cancelled("slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            42
        });
        tasks.cancel();
        assert_eq!(interrupted.join().await.unwrap(), None);
    }

    #[tokio::test]
    async fn panicking_tasks_surface_as_errors() {
        let tasks = Tasks::new();
        let handle = tasks.spawn("doomed", async { panic!("boom") });
        let error = handle.join().await.unwrap_err();
        assert!(error.to_string().contains("doomed"), "{error}");
    }
}
