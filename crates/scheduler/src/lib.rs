//! Everything the system does without being asked.
//!
//! A [`Scheduler`](aik_api::scheduler::Scheduler) turns a
//! [`Trigger`](aik_api::scheduler::Trigger) into a call to a
//! [`JobHandler`](aik_api::scheduler::JobHandler): periodic maintenance, a reminder, a
//! background agent run, a reaction to something that happened elsewhere in the kernel.
//!
//! # What this crate contains
//!
//! * [`JobScheduler`] — the engine. One implementation, wired either with a
//!   [`JobStore`] or without one; there is deliberately no separate "in-memory scheduler",
//!   because every rule below has to hold identically either way.
//! * [`RedbJobStore`] — the durable mirror, in the kernel's shared database.
//! * [`SchedulerComponent`] and [`RedbSchedulerComponent`] — the kernel wiring for each.
//!
//! ```
//! use std::time::Duration;
//! use aik_api::execution::ExecutionContext;
//! use aik_api::scheduler::{JobSpec, Scheduler, Trigger};
//! use aik_core::event::EventBus;
//! use aik_core::task::Tasks;
//! use aik_scheduler::{JobScheduler, SchedulerRuntime};
//! # use std::sync::Arc;
//! # use aik_core::clock::SystemClock;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> aik_core::Result<()> {
//! let runtime = SchedulerRuntime::new(
//!     "scheduler.jobs",
//!     Arc::new(SystemClock),
//!     EventBus::default(),
//!     Tasks::new(),
//! );
//! let scheduler = Arc::new(JobScheduler::volatile(runtime));
//! scheduler.start(Default::default()).await?;
//!
//! let cx = ExecutionContext::new();
//! scheduler
//!     .schedule(
//!         JobSpec::new(
//!             "sweep",
//!             Trigger::Every { interval: Duration::from_secs(60) },
//!             "jobs.sweep",
//!         ),
//!         &cx,
//!     )
//!     .await?;
//!
//! assert_eq!(scheduler.list(&cx).await?.len(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! # The guarantees, stated once
//!
//! Unattended work is the kind nobody watches, so what it does and does not promise has to be
//! written down rather than inferred from the implementation.
//!
//! ## Volatile and persistent jobs
//!
//! The schedule lives in memory and always does. A [`JobStore`] is a *durable mirror* of the
//! [`persistent`](aik_api::scheduler::JobSpec::persistent) subset of it, never the schedule
//! itself, which is what lets one engine serve both wirings: a scheduler with no store
//! behaves exactly like one whose store happens to hold nothing.
//!
//! A volatile job never reaches the disk, however the scheduler is wired. A persistent job
//! asked of a scheduler that has no store is refused with
//! [`Error::Unsupported`](aik_core::Error::Unsupported) — never accepted and quietly
//! forgotten, which is the one outcome that would let a deployment believe its nightly job
//! exists.
//!
//! ## What `persistent: true` buys
//!
//! * The **definition** survives a restart: id, trigger, handler, payload, retry policy,
//!   deadline, and the principal that scheduled it.
//! * The **schedule position** survives: `next_run` and `last_run`, so a periodic job resumes
//!   its cadence instead of restarting its phase at boot.
//! * **Cancellation and replacement are durable.** A `cancel` that returned `Ok(true)` means
//!   the job is not there after a restart either.
//!
//! It does **not** make a *run* durable. A firing interrupted by a crash is not resumed, a
//! pending retry is not resumed, and a job is not a work queue. What survives is when the job
//! fires, not any particular firing seeing itself through.
//!
//! ## At-most-once
//!
//! A firing is claimed by writing the advanced schedule **before** the handler is called. A
//! process that dies mid-firing therefore comes back to a job whose next occurrence is in the
//! future: the firing is lost rather than repeated.
//!
//! That is the deliberate side to fail on. The scheduler cannot see what a handler did — a
//! message sent, a model called and paid for, a file deleted — so it cannot make a repeat
//! safe, while a lost firing is visible in the events and recoverable by the next occurrence.
//! A job that needs at-least-once must make its handler idempotent and say so itself.
//!
//! Duplicate firings are impossible in the other two directions as well: exclusion within the
//! process is an in-memory lock (below), and across processes redb hands the database to
//! exactly one of them, so a second scheduler cannot even open the schedule.
//!
//! ## Overlap
//!
//! **One firing per job at a time.** A firing that comes due while the previous one is still
//! running is skipped, and says so with
//! [`JobSkipped`](aik_api::scheduler::JobSkipped)`{ reason: AlreadyRunning }`. The schedule
//! still advances, so a slow job falls behind rather than accumulating a backlog that arrives
//! all at once when it finally catches up.
//!
//! There is no knob for this. Queueing overlapping firings needs a bound and a policy for
//! what to do when the bound is reached, which is a larger decision than it looks and is
//! better made when something actually needs it.
//!
//! Exclusion is held in memory, next to the tasks it describes — never in the database. A row
//! saying "running" is a claim about a process that may no longer exist, and every mechanism
//! for repairing that (leases, expiry, recovery) can strand a job that then never runs again.
//!
//! ## Retries
//!
//! Off by default. [`RetryPolicy`](aik_api::scheduler::RetryPolicy) adds attempts with an
//! exponentially doubling, capped backoff. A retry belongs to the *firing* that failed: same
//! [`RunId`](aik_api::scheduler::RunId), same correlation id, same exclusion slot — so a job
//! backing off cannot be overtaken by its own next occurrence.
//!
//! Retries are in-process. A retry pending when the process dies is gone, and is not resumed
//! by a restart; the job's next scheduled firing is unaffected. For a repeating trigger, the
//! next occurrence is usually the retry worth having anyway.
//!
//! ## Missed firings
//!
//! At startup a persistent job may be due in the past. At most **one** missed firing runs,
//! and only if it came due within the catch-up window
//! ([`DEFAULT_CATCH_UP_WINDOW`], overridable per component or in configuration). Anything
//! older is reported as [`JobSkipped`](aik_api::scheduler::JobSkipped)`{ reason: Missed }`
//! and the schedule rolls forward to the next future occurrence.
//!
//! Running every missed occurrence is a stampede; running none of them silently is a system
//! that looks healthy while nothing happens. One, reported, bounded by age, is the only
//! option that is neither.
//!
//! ## Cancellation
//!
//! Two different things, kept apart:
//!
//! * [`cancel`](aik_api::scheduler::Scheduler::cancel) removes the job from the schedule,
//!   durably if it was persistent. No further firings, and any pending retry is abandoned.
//! * The **firing in flight**, if there is one, is cancelled *cooperatively*: its
//!   [`ExecutionContext`](aik_api::execution::ExecutionContext) token is cancelled and the
//!   handler is expected to notice. Nothing is aborted, so a handler that ignores
//!   cancellation runs to completion and its outcome is still published.
//!
//! **Replacing** a job — scheduling one whose id is already in use — deliberately does *not*
//! touch the firing in flight. Replacement says something about when the job runs next, not
//! about the run happening now, and a handler reprogramming its own schedule from inside its
//! own firing would otherwise be asking to be killed. The replacement still cannot overlap
//! the old run, because exclusion is keyed by job id rather than by definition.
//!
//! ## Deadlines
//!
//! [`JobSpec::timeout`](aik_api::scheduler::JobSpec::timeout) becomes the firing's
//! [`deadline`](aik_api::execution::ExecutionContext::deadline) *and* is enforced: the attempt
//! is cancelled and reported as [`Error::Timeout`](aik_core::Error::Timeout), and it counts as
//! a failure like any other, so a retry policy applies to it.
//!
//! Enforcement is a dropped future and a cancelled token, not an abort. Work a handler put on
//! a blocking thread — a database transaction, most obviously — cannot be interrupted at all
//! and will finish and commit whatever the deadline said. That is a property of blocking work
//! rather than something a scheduler can paper over.
//!
//! ## Shutdown
//!
//! Stopping the component stops the driver, so nothing new fires, and cancels every firing in
//! flight. The kernel then waits for them under the one shutdown deadline it applies to
//! everything. Scheduling after shutdown is [`Error::Cancelled`](aik_core::Error::Cancelled)
//! rather than a silent acceptance of a job nothing will ever run.
//!
//! ## Events
//!
//! Every firing publishes [`JobStarted`](aik_api::scheduler::JobStarted), then exactly one of
//! [`JobCompleted`](aik_api::scheduler::JobCompleted),
//! [`JobFailed`](aik_api::scheduler::JobFailed) or
//! [`JobCancelled`](aik_api::scheduler::JobCancelled) — per attempt, all under one run id. A
//! firing that never happened publishes [`JobSkipped`](aik_api::scheduler::JobSkipped),
//! because "the job did not run" is exactly as interesting as "the job failed".
//!
//! Two terminal events have no `JobStarted` before them, because nothing started: a job whose
//! handler is not registered (a `JobFailed` at attempt zero), and a firing dispatched into a
//! shutdown that reached it first (a `JobCancelled` at attempt zero). A consumer pairing
//! starts with outcomes has to tolerate both rather than assume a start is always there.
//!
//! No event carries the job's payload or anything a handler produced, for the reason
//! [`audit`](aik_api::audit#what-these-events-must-never-carry) gives at length.
//!
//! # Security
//!
//! A job belongs to the principal whose context scheduled it. The owner is taken from the
//! [`ExecutionContext`](aik_api::execution::ExecutionContext) and never from the
//! [`JobSpec`](aik_api::scheduler::JobSpec), so a specification written by a model — or read
//! out of a configuration file — cannot choose whose authority it runs with. Naming another
//! principal's job is
//! [`Error::PermissionDenied`](aik_core::Error::PermissionDenied); enumerating simply does not
//! return it, because an error would confirm it exists. The rule itself is
//! [`Principal::may_act_for`](aik_api::permission::Principal::may_act_for), shared with the
//! memory and context stores so the three cannot answer the same question differently.
//!
//! A firing runs as [`RUN_PRINCIPAL`] acting on behalf of the owner — the system, doing
//! something for someone. What that buys, and what it deliberately does not: delegation does
//! not compound, so a job scheduled by an agent that was itself acting for a user can act for
//! the agent and not for the user.
//!
//! # What this deliberately does not do
//!
//! * **No cron.** [`Trigger::Cron`](aik_api::scheduler::Trigger::Cron) is refused at
//!   scheduling time with [`Error::Unsupported`](aik_core::Error::Unsupported) — which is what
//!   the contract asks of a scheduler that cannot parse an expression. Defining a dialect
//!   means picking between incompatible conventions and carrying a parser for the choice;
//!   `Every` covers the periodic case, and the calendar case should be designed when something
//!   needs a calendar.
//! * **No distribution.** One process, one schedule. The lock redb takes on the database file
//!   is what enforces it, and it is why none of the leasing machinery a distributed scheduler
//!   needs appears here.
//! * **No chaining off its own events.** A job triggered by an event the scheduler itself
//!   published would retrigger on its own completion for ever, so the scheduler ignores its
//!   own output when matching
//!   [`Trigger::OnEvent`](aik_api::scheduler::Trigger::OnEvent). Chaining needs loop detection
//!   and is better added deliberately.
//! * **It does not decide what is worth scheduling.** What to run, how often, and what to do
//!   about a job that keeps failing is the caller's judgement, not the scheduler's.

mod component;
mod events;
mod owner;
mod persistent;
mod runner;
mod scheduler;
mod state;
mod store;

pub use component::{DEFAULT_COMPONENT_ID, RedbSchedulerComponent, SchedulerComponent};
pub use owner::RUN_PRINCIPAL;
pub use persistent::RedbJobStore;
pub use scheduler::{DEFAULT_CATCH_UP_WINDOW, JobScheduler, SchedulerRuntime};
pub use state::JobState;
pub use store::JobStore;
