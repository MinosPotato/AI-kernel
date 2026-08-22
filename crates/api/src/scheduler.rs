//! Scheduling contracts.
//!
//! A [`Scheduler`] turns a [`Trigger`] into a call to a [`JobHandler`]. This is the seam
//! for everything the system does without being asked: periodic maintenance, reminders,
//! background agent runs, reactions to events.
//!
//! [`Trigger::OnEvent`] is what makes the system event-driven rather than merely
//! time-driven — a job can be attached to any event name on the kernel bus, including
//! events from subsystems the scheduler knows nothing about.
//!
//! # Jobs are owned
//!
//! A job belongs to the principal whose [`ExecutionContext`] first scheduled it, exactly as a
//! [memory record](crate::memory#records-are-owned) belongs to the principal that first
//! stored it. The owner is stamped by the scheduler from the context and is never taken from
//! the [`JobSpec`], so a job specification written by a model — or read out of a
//! configuration file — cannot choose whose authority it runs with.
//!
//! [`Scheduler::schedule`] and [`Scheduler::cancel`] name one job, so a caller that may not
//! act for its owner is refused with
//! [`Error::PermissionDenied`](aik_core::Error::PermissionDenied). [`Scheduler::list`]
//! enumerates, so jobs the caller may not act for are simply absent from the result rather
//! than an error that would confirm they exist.
//!
//! [`Principal::may_act_for`](crate::permission::Principal::may_act_for) is the single
//! definition of "may act for", shared with the memory and context stores.
//!
//! # What a firing carries
//!
//! A firing runs in its own [`ExecutionContext`]: a fresh
//! [`CorrelationId`], the job's [`timeout`](JobSpec::timeout) as the deadline, a cancellation
//! token the scheduler holds, and a principal derived from the job's owner. It is the
//! implementation's business exactly how the run principal is derived — see the
//! implementation's own documentation — but it must be derived from the owner recorded at
//! scheduling time and never from anything in the spec.

use std::time::Duration;

use aik_core::Event;
use aik_core::Result;
use aik_core::clock::Timestamp;
use aik_core::id::{CorrelationId, EventName};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;
use crate::permission::{PrincipalId, PrincipalKind};

aik_core::string_id! {
    /// Names a scheduled job. Stable, so a job can be replaced or cancelled across restarts.
    pub JobId
}

aik_core::uuid_id! {
    /// Identifies one firing of a job.
    ///
    /// A job fires many times; each firing is one run, with its own identifier, its own
    /// [`ExecutionContext`] and its own lifecycle events. Retries of a failed attempt belong
    /// to the *same* run — see [`RetryPolicy`] — so a run id identifies the firing, not the
    /// attempt.
    pub RunId
}

/// When a job should run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Once, at an absolute time.
    At {
        /// When.
        timestamp: Timestamp,
    },
    /// Once, after a delay measured from scheduling time.
    After {
        /// How long to wait.
        #[serde(with = "duration_millis")]
        delay: Duration,
    },
    /// Repeatedly, at a fixed interval.
    Every {
        /// The interval.
        #[serde(with = "duration_millis")]
        interval: Duration,
    },
    /// On a cron expression.
    ///
    /// The kernel does not define the dialect; the implementing scheduler does, and should
    /// reject expressions it cannot parse at scheduling time rather than at fire time.
    Cron {
        /// The expression.
        expression: String,
    },
    /// Whenever a kernel event is published.
    OnEvent {
        /// The event's wire name.
        event: EventName,
    },
}

/// What to do when a firing's handler returns an error.
///
/// A retry belongs to the firing that failed: it keeps the run's identity, and it holds
/// whatever exclusion the scheduler applies to concurrent firings of the same job, so a
/// failing job backing off cannot be overtaken by its own next occurrence.
///
/// Retries are an in-process affair. A scheduler that persists jobs is not thereby obliged to
/// persist a pending retry, and should say so: the durable guarantee is about *when a job
/// fires*, not about seeing one firing through to a successful attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    /// How many *additional* attempts to make after the first one fails.
    ///
    /// Zero — the default — means a failed firing is simply a failed firing. For a repeating
    /// trigger that is often the right answer already, because the next occurrence is itself
    /// the retry.
    pub attempts: u32,
    /// How long to wait before the first retry.
    #[serde(with = "duration_millis")]
    pub backoff: Duration,
    /// The ceiling the backoff doubles up to.
    #[serde(with = "duration_millis")]
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

impl RetryPolicy {
    /// The default backoff before a first retry.
    pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(1);

    /// The default ceiling the backoff doubles up to.
    pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

    /// No retries: one attempt, and a failure is final for that firing.
    pub const fn none() -> Self {
        Self {
            attempts: 0,
            backoff: Self::DEFAULT_BACKOFF,
            max_backoff: Self::DEFAULT_MAX_BACKOFF,
        }
    }

    /// Retries up to `attempts` times, doubling from [`RetryPolicy::DEFAULT_BACKOFF`].
    pub const fn attempts(attempts: u32) -> Self {
        Self {
            attempts,
            ..Self::none()
        }
    }

    /// Sets the delay before the first retry.
    #[must_use]
    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sets the ceiling the backoff doubles up to.
    #[must_use]
    pub const fn with_max_backoff(mut self, max_backoff: Duration) -> Self {
        self.max_backoff = max_backoff;
        self
    }

    /// How long to wait before attempt number `attempt`, counting the first attempt as zero.
    ///
    /// Doubles from [`RetryPolicy::backoff`], capped at [`RetryPolicy::max_backoff`]. Returns
    /// `None` when `attempt` is past what this policy allows, which is what tells a caller to
    /// stop rather than to wait forever.
    ///
    /// ```
    /// use std::time::Duration;
    /// use aik_api::scheduler::RetryPolicy;
    ///
    /// let policy = RetryPolicy::attempts(3)
    ///     .with_backoff(Duration::from_secs(1))
    ///     .with_max_backoff(Duration::from_secs(3));
    ///
    /// assert_eq!(policy.delay_before(0), None, "the first attempt is not a retry");
    /// assert_eq!(policy.delay_before(1), Some(Duration::from_secs(1)));
    /// assert_eq!(policy.delay_before(2), Some(Duration::from_secs(2)));
    /// assert_eq!(policy.delay_before(3), Some(Duration::from_secs(3)), "capped");
    /// assert_eq!(policy.delay_before(4), None, "three retries is three retries");
    /// ```
    pub fn delay_before(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 || attempt > self.attempts {
            return None;
        }
        let doubled = self
            .backoff
            .checked_mul(1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX))
            .unwrap_or(self.max_backoff);
        Some(doubled.min(self.max_backoff))
    }
}

/// A job to be scheduled.
///
/// Built through [`JobSpec::new`] and the `with_*` methods rather than as a struct literal,
/// so that a field added later is not a breaking change for every caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The job's stable name.
    pub id: JobId,
    /// When it runs.
    pub trigger: Trigger,
    /// Which handler runs it, as registered in the kernel registry.
    pub handler: aik_core::ComponentId,
    /// Handler-specific data.
    ///
    /// Opaque to the scheduler, and deliberately never included in a scheduler event: it is
    /// caller-authored content of unknown sensitivity, and the reasoning
    /// [`audit`](crate::audit#what-these-events-must-never-carry) gives for keeping tool
    /// arguments out of audit records applies here unchanged.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// Whether the job survives a restart. Implementations without persistence should
    /// reject `true` rather than quietly forget.
    #[serde(default)]
    pub persistent: bool,
    /// What to do when a firing fails.
    #[serde(default)]
    pub retry: RetryPolicy,
    /// How long one firing may take before it is cancelled, if there is a limit.
    ///
    /// Becomes the [`deadline`](ExecutionContext::deadline) of the firing's context, and is
    /// enforced by the scheduler rather than merely advertised: a handler that overruns is
    /// cancelled and the firing is reported as
    /// [`Error::Timeout`](aik_core::Error::Timeout).
    #[serde(default, with = "duration_millis_option")]
    pub timeout: Option<Duration>,
}

impl JobSpec {
    /// Describes a volatile job with no retries and no deadline.
    pub fn new(
        id: impl Into<JobId>,
        trigger: Trigger,
        handler: impl Into<aik_core::ComponentId>,
    ) -> Self {
        Self {
            id: id.into(),
            trigger,
            handler: handler.into(),
            payload: Value::Null,
            persistent: false,
            retry: RetryPolicy::none(),
            timeout: None,
        }
    }

    /// Attaches handler-specific data.
    #[must_use]
    pub fn with_payload(mut self, payload: impl Into<Value>) -> Self {
        self.payload = payload.into();
        self
    }

    /// Asks for the job to survive a restart.
    #[must_use]
    pub fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    /// Sets what happens when a firing fails.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Limits how long one firing may take.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// A job as the scheduler currently sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJob {
    /// What was scheduled.
    pub spec: JobSpec,
    /// The principal the job belongs to, and whose authority its firings carry.
    pub owner: PrincipalId,
    /// When it will next run, if that is knowable.
    ///
    /// `None` for a [`Trigger::OnEvent`] job, whose next run depends on something nobody can
    /// predict, and for a one-shot job that has already run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<Timestamp>,
    /// When it last ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Timestamp>,
}

/// Runs scheduled jobs.
#[async_trait]
pub trait Scheduler: Send + Sync + 'static {
    /// Schedules a job, replacing any existing job with the same id.
    ///
    /// A first schedule records `cx`'s principal as the job's owner. A replacement keeps the
    /// owner it already had and is refused with
    /// [`Error::PermissionDenied`](aik_core::Error::PermissionDenied) unless `cx` may act for
    /// them, so replacing a job is never a way to take it over.
    async fn schedule(&self, spec: JobSpec, cx: &ExecutionContext) -> Result<()>;

    /// Cancels a job, returning whether it existed.
    ///
    /// Stops future firings. What it does to a firing already in flight is the
    /// implementation's to define and to document.
    async fn cancel(&self, id: &JobId, cx: &ExecutionContext) -> Result<bool>;

    /// Lists the jobs `cx` may act for.
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ScheduledJob>>;
}

/// Does the work when a job fires.
///
/// Handlers are registered in the kernel registry under `dyn JobHandler` and referenced by
/// [`JobSpec::handler`], so a job specification stays serialisable — which is what allows
/// jobs to be persisted or written by a user in configuration.
#[async_trait]
pub trait JobHandler: Send + Sync + 'static {
    /// Runs the job.
    ///
    /// A firing that overruns should honour `cx`'s cancellation; the scheduler decides
    /// what to do about overlapping runs.
    async fn run(&self, job: &JobSpec, cx: &ExecutionContext) -> Result<()>;
}

/// The fields every job lifecycle event carries.
///
/// Flattened into each event rather than nested, so a firehose consumer filtering on `job`
/// does not have to know which event it is looking at first.
///
/// # What these events do not carry
///
/// [`JobSpec::payload`], and nothing that came out of a handler. A scheduler event says which
/// job fired, for whom, and how it ended; the contents of the work are the job's business.
/// This is the same rule, for the same reason, as
/// [`audit`](crate::audit#what-these-events-must-never-carry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEvent {
    /// The job this happened to.
    pub job: JobId,
    /// The firing it happened to.
    pub run: RunId,
    /// The handler the job names.
    pub handler: aik_core::ComponentId,
    /// The operation the firing runs as, tying its events to everything it does.
    pub correlation: CorrelationId,
    /// When it happened, by the kernel clock.
    pub timestamp: Timestamp,
    /// The principal the job belongs to.
    pub owner: PrincipalId,
    /// What kind of actor the owner is.
    pub owner_kind: PrincipalKind,
}

/// A firing has begun: the handler is about to be called.
///
/// Published once per attempt, so a firing that retries twice produces three of these under
/// one [`RunId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStarted {
    /// Which job, which firing, for whom.
    #[serde(flatten)]
    pub event: JobEvent,
    /// Which attempt this is, counting the first as zero.
    pub attempt: u32,
    /// When the firing was due, which is not when it started if the scheduler was busy.
    pub scheduled_for: Timestamp,
}

impl Event for JobStarted {
    const NAME: &'static str = "scheduler.job_started";
}

/// A firing's handler returned successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCompleted {
    /// Which job, which firing, for whom.
    #[serde(flatten)]
    pub event: JobEvent,
    /// Which attempt succeeded, counting the first as zero.
    pub attempt: u32,
    /// How long the successful attempt took, in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
}

impl Event for JobCompleted {
    const NAME: &'static str = "scheduler.job_completed";
}

/// A firing's handler failed.
///
/// Published once per failed attempt. [`JobFailed::will_retry`] is what distinguishes a
/// setback from the end of the firing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFailed {
    /// Which job, which firing, for whom.
    #[serde(flatten)]
    pub event: JobEvent,
    /// Which attempt failed, counting the first as zero.
    pub attempt: u32,
    /// How long the failed attempt took, in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// The kernel error classification, e.g. `timeout` or `not_found`.
    ///
    /// The classification as well as the message, so a consumer can alert on a class of
    /// failure without parsing prose.
    pub kind: String,
    /// What went wrong.
    ///
    /// Handler-authored text. Unlike a payload this is diagnostic by intent, but a consumer
    /// shipping these somewhere durable should treat it as it would any other message
    /// produced by code the scheduler does not control.
    pub error: String,
    /// Whether another attempt is coming.
    pub will_retry: bool,
}

impl Event for JobFailed {
    const NAME: &'static str = "scheduler.job_failed";
}

/// A firing stopped because it was cancelled: the job was cancelled, or the system is
/// shutting down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancelled {
    /// Which job, which firing, for whom.
    #[serde(flatten)]
    pub event: JobEvent,
    /// Which attempt was interrupted, counting the first as zero.
    pub attempt: u32,
}

impl Event for JobCancelled {
    const NAME: &'static str = "scheduler.job_cancelled";
}

/// Why a firing that was due never happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A previous firing of the same job was still running.
    AlreadyRunning,
    /// The firing came due while the system was not running, and was too old to catch up on.
    Missed,
}

/// A firing that was due did not happen, and why.
///
/// Published rather than logged because "the job did not run" is exactly as operationally
/// interesting as "the job failed", and a system that only reports the latter looks healthy
/// while nothing happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSkipped {
    /// Which job, which firing, for whom.
    ///
    /// The [`RunId`] identifies the firing that did not happen, so it appears in no other
    /// event.
    #[serde(flatten)]
    pub event: JobEvent,
    /// When the skipped firing was due.
    pub scheduled_for: Timestamp,
    /// Why it was skipped.
    pub reason: SkipReason,
}

impl Event for JobSkipped {
    const NAME: &'static str = "scheduler.job_skipped";
}

/// Serialises a `Duration` as whole milliseconds, matching [`Timestamp`].
mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        duration: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(duration.as_millis() as u64)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

/// The same, for an optional `Duration`, which serialises as `null` when absent.
mod duration_millis_option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match duration {
            Some(duration) => serializer.serialize_some(&(duration.as_millis() as u64)),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_round_trip_as_tagged_json() {
        let trigger = Trigger::Every {
            interval: Duration::from_secs(30),
        };
        let json = serde_json::to_value(&trigger).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "type": "every", "interval": 30_000 })
        );
        assert_eq!(serde_json::from_value::<Trigger>(json).unwrap(), trigger);
    }

    #[test]
    fn a_minimal_spec_omits_everything_it_did_not_ask_for() {
        let spec = JobSpec::new(
            "nightly",
            Trigger::Every {
                interval: Duration::from_secs(60),
            },
            "jobs.nightly",
        );
        let json = serde_json::to_value(&spec).unwrap();

        assert_eq!(json["id"], serde_json::json!("nightly"));
        assert_eq!(json["persistent"], serde_json::json!(false));
        assert_eq!(json["timeout"], serde_json::Value::Null);
        assert!(json.get("payload").is_none(), "a null payload is omitted");
        assert_eq!(json["retry"]["attempts"], serde_json::json!(0));
    }

    #[test]
    fn a_spec_round_trips_with_every_option_set() {
        let spec = JobSpec::new(
            "digest",
            Trigger::At {
                timestamp: Timestamp::from_millis(1_000),
            },
            "jobs.digest",
        )
        .with_payload(serde_json::json!({ "channel": "email" }))
        .persistent(true)
        .with_retry(RetryPolicy::attempts(2).with_backoff(Duration::from_millis(250)))
        .with_timeout(Duration::from_secs(30));

        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["timeout"], serde_json::json!(30_000));
        assert_eq!(json["retry"]["backoff"], serde_json::json!(250));
        assert_eq!(serde_json::from_value::<JobSpec>(json).unwrap(), spec);
    }

    #[test]
    fn a_spec_written_without_the_optional_fields_still_deserialises() {
        // What a job written by hand in configuration, or persisted by an earlier build,
        // looks like. The defaults have to be the conservative ones: no retries, no deadline.
        let spec: JobSpec = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "trigger": { "type": "after", "delay": 5_000 },
            "handler": "jobs.legacy",
        }))
        .unwrap();

        assert_eq!(spec.retry, RetryPolicy::none());
        assert_eq!(spec.timeout, None);
        assert!(!spec.persistent);
    }

    #[test]
    fn no_retries_means_no_delay_is_ever_offered() {
        assert_eq!(RetryPolicy::none().delay_before(1), None);
    }

    #[test]
    fn a_huge_attempt_count_saturates_rather_than_overflowing() {
        let policy = RetryPolicy::attempts(u32::MAX)
            .with_backoff(Duration::from_secs(1))
            .with_max_backoff(Duration::from_secs(60));
        assert_eq!(policy.delay_before(64), Some(Duration::from_secs(60)));
        assert_eq!(policy.delay_before(u32::MAX), Some(Duration::from_secs(60)));
    }

    #[test]
    fn job_events_flatten_so_a_consumer_can_filter_without_knowing_the_type() {
        let started = JobStarted {
            event: JobEvent {
                job: JobId::new("nightly"),
                run: RunId::new(),
                handler: aik_core::ComponentId::new("jobs.nightly"),
                correlation: CorrelationId::new(),
                timestamp: Timestamp::from_millis(7),
                owner: PrincipalId::new("alice"),
                owner_kind: PrincipalKind::User,
            },
            attempt: 0,
            scheduled_for: Timestamp::from_millis(5),
        };

        let json = serde_json::to_value(&started).unwrap();
        assert_eq!(json["job"], serde_json::json!("nightly"));
        assert_eq!(json["owner"], serde_json::json!("alice"));
        assert_eq!(json["attempt"], serde_json::json!(0));
        assert_eq!(
            serde_json::from_value::<JobStarted>(json).unwrap(),
            started,
            "an event has to survive the firehose it was designed for"
        );
    }
}
