//! One firing, from the moment it is dispatched to the moment it stops mattering.
//!
//! # What a firing is
//!
//! A [`RunId`], a handler call, and possibly some retries of that call. Retries belong to the
//! firing rather than being new firings: they share the run id, the correlation id and the
//! job's exclusion slot, so a job that is backing off cannot be overtaken by its own next
//! occurrence, and an observer counting runs counts occurrences rather than attempts.
//!
//! # Cancellation
//!
//! Two tokens, nested:
//!
//! * the **run token**, held by the scheduler, cancelled when the job is cancelled or the
//!   kernel shuts down. It ends the firing: any pending retry is abandoned rather than waited
//!   out.
//! * the **attempt token**, a child of it, handed to the handler in its
//!   [`ExecutionContext`]. The run token reaches it, and so does this firing's own deadline —
//!   which is why the deadline expiring is distinguishable from the job being cancelled,
//!   rather than both arriving as "somebody cancelled you".
//!
//! Neither aborts anything. A handler is asked to stop and is expected to notice; what it
//! does inside [`spawn_blocking`](tokio::task::spawn_blocking) cannot be interrupted at all,
//! so a handler running a database transaction there will finish and commit it whatever the
//! deadline said. That is a property of blocking work, not something a scheduler can paper
//! over, and a handler that needs to be interruptible has to check the token itself.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId};
use aik_api::scheduler::{
    JobCancelled, JobCompleted, JobFailed, JobHandler, JobId, JobSpec, JobStarted, RunId,
};
use aik_core::clock::Timestamp;
use aik_core::{Error, Result};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::events::Publisher;
use crate::owner::run_principal;

/// One firing, as the scheduler needs to see it from outside the task running it.
#[derive(Debug, Clone)]
pub(crate) struct RunSlot {
    /// Which firing this is.
    pub run: RunId,
    /// Cancelling this asks the firing to stop and abandons any pending retry.
    pub token: CancellationToken,
    /// Who the job belongs to.
    ///
    /// Carried here as well as in the schedule because a one-shot job leaves the schedule the
    /// moment it is claimed, and cancelling a firing still has to be authorised against
    /// somebody.
    pub owner: PrincipalId,
}

/// The jobs currently executing, and how to ask each of them to stop.
///
/// In memory, deliberately. A row in a database saying "running" is a claim about a process
/// that may no longer exist — it has to be leased, expired and recovered, and every one of
/// those mechanisms can strand a job that will then never run again. Exclusion is only ever
/// needed *within* one scheduler, because redb gives exactly one process the database at a
/// time, so the honest place to keep it is next to the tasks it describes.
pub(crate) type Running = Arc<Mutex<std::collections::HashMap<JobId, RunSlot>>>;

/// Holds a job's exclusion slot for as long as its firing lives.
///
/// Releasing on `Drop` rather than at the end of the run loop is what makes the slot
/// impossible to leak: a handler that panics releases it, and so does a run task dropped
/// mid-await by shutdown.
#[derive(Debug)]
pub(crate) struct RunGuard {
    running: Running,
    job: JobId,
    run: RunId,
}

impl RunGuard {
    /// Records `slot` as the firing occupying `job`'s slot.
    pub(crate) fn claim(running: Running, job: JobId, slot: RunSlot) -> Self {
        let run = slot.run;
        running
            .lock()
            .expect("the running-job lock is never held across a panic")
            .insert(job.clone(), slot);
        Self { running, job, run }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut running = self
            .running
            .lock()
            .expect("the running-job lock is never held across a panic");
        // Only if it is still *this* firing's slot. Cancelling a job leaves the entry in
        // place so that its replacement cannot start while the old run winds down, and a
        // guard that removed unconditionally would hand that guarantee away.
        if running
            .get(&self.job)
            .is_some_and(|slot| slot.run == self.run)
        {
            running.remove(&self.job);
        }
    }
}

/// Everything one firing needs to know about itself.
#[derive(Debug)]
pub(crate) struct Firing {
    /// What is being run.
    pub spec: JobSpec,
    /// Who it runs for.
    pub owner: Principal,
    /// This firing's identity.
    pub run: RunId,
    /// The occurrence this firing is for, which is not when it actually started.
    pub scheduled_for: Timestamp,
    /// Cancelled when the job is cancelled or the kernel stops.
    pub token: CancellationToken,
}

/// Runs one firing to a conclusion, publishing what happened at every step.
///
/// Never returns an error: there is nobody to return one to. A firing that fails says so on
/// the bus, which is the only channel an unattended job has.
pub(crate) async fn execute(
    publisher: Publisher,
    handler: Arc<dyn JobHandler>,
    firing: Firing,
    _slot: RunGuard,
) {
    let correlation = aik_core::id::CorrelationId::new();
    let mut attempt = 0u32;

    loop {
        if firing.token.is_cancelled() {
            publish_cancelled(&publisher, &firing, correlation, attempt);
            return;
        }

        let cx = context(&firing, correlation, publisher.now());
        publisher.publish(
            JobStarted {
                event: publisher.job_event(
                    &firing.spec.id,
                    &firing.spec.handler,
                    &firing.owner,
                    firing.run,
                    correlation,
                ),
                attempt,
                scheduled_for: firing.scheduled_for,
            },
            correlation,
        );

        let started = Instant::now();
        let outcome = attempt_once(handler.as_ref(), &firing.spec, &cx).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let error = match outcome {
            Ok(()) => {
                publisher.publish(
                    JobCompleted {
                        event: publisher.job_event(
                            &firing.spec.id,
                            &firing.spec.handler,
                            &firing.owner,
                            firing.run,
                            correlation,
                        ),
                        attempt,
                        duration_ms,
                    },
                    correlation,
                );
                return;
            }
            Err(error) => error,
        };

        // The run token, not the attempt token: a deadline that expired cancelled only the
        // latter, and is a failure this job may still retry rather than an instruction to
        // stop.
        if firing.token.is_cancelled() {
            publish_cancelled(&publisher, &firing, correlation, attempt);
            return;
        }

        let delay = firing.spec.retry.delay_before(attempt + 1);
        tracing::warn!(
            job = %firing.spec.id,
            run = %firing.run,
            attempt,
            will_retry = delay.is_some(),
            %error,
            "scheduled job failed"
        );
        publisher.publish(
            JobFailed {
                event: publisher.job_event(
                    &firing.spec.id,
                    &firing.spec.handler,
                    &firing.owner,
                    firing.run,
                    correlation,
                ),
                attempt,
                duration_ms,
                kind: format!("{:?}", error.kind()).to_lowercase(),
                error: error.to_string(),
                will_retry: delay.is_some(),
            },
            correlation,
        );

        let Some(delay) = delay else { return };

        tokio::select! {
            () = firing.token.cancelled() => {
                publish_cancelled(&publisher, &firing, correlation, attempt);
                return;
            }
            () = tokio::time::sleep(delay) => {}
        }
        attempt += 1;
    }
}

/// Calls the handler once, enforcing the job's deadline if it has one.
///
/// A deadline that expires stops waiting, which drops the handler's future at its next await
/// point, and then cancels the attempt's context. That order matters and is worth being plain
/// about: the handler's own future is already gone by the time the token is cancelled, so a
/// handler cannot use its context to clean up after its *own* deadline. What the cancellation
/// still reaches is everything the handler cloned the token into — a spawned task, blocking
/// work checking it — which is the part a dropped future does not stop.
///
/// The failure is reported as [`Error::Timeout`] so that "took too long" is distinguishable
/// from whatever the handler would have returned.
async fn attempt_once(
    handler: &dyn JobHandler,
    spec: &JobSpec,
    cx: &ExecutionContext,
) -> Result<()> {
    let Some(timeout) = spec.timeout else {
        return handler.run(spec, cx).await;
    };
    match tokio::time::timeout(timeout, handler.run(spec, cx)).await {
        Ok(result) => result,
        Err(_) => {
            cx.cancellation.cancel();
            Err(Error::Timeout(timeout))
        }
    }
}

/// The context one attempt runs in.
///
/// A fresh cancellation token per attempt, child of the run's, so that a deadline cancelling
/// attempt one does not arrive already-cancelled at attempt two. The correlation id is
/// deliberately *not* fresh: every attempt is the same logical firing.
fn context(
    firing: &Firing,
    correlation: aik_core::id::CorrelationId,
    now: Timestamp,
) -> ExecutionContext {
    let mut attributes = Map::new();
    attributes.insert("job".into(), Value::String(firing.spec.id.to_string()));
    attributes.insert("run".into(), Value::String(firing.run.to_string()));

    ExecutionContext {
        correlation,
        principal: Some(run_principal(&firing.owner.id)),
        deadline: firing
            .spec
            .timeout
            .map(|timeout| now.saturating_add(timeout)),
        cancellation: firing.token.child_token(),
        attributes,
    }
}

fn publish_cancelled(
    publisher: &Publisher,
    firing: &Firing,
    correlation: aik_core::id::CorrelationId,
    attempt: u32,
) {
    publisher.publish(
        JobCancelled {
            event: publisher.job_event(
                &firing.spec.id,
                &firing.spec.handler,
                &firing.owner,
                firing.run,
                correlation,
            ),
            attempt,
        },
        correlation,
    );
}
