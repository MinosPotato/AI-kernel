//! Scheduling contracts.
//!
//! A [`Scheduler`] turns a [`Trigger`] into a call to a [`JobHandler`]. This is the seam
//! for everything the system does without being asked: periodic maintenance, reminders,
//! background agent runs, reactions to events.
//!
//! [`Trigger::OnEvent`] is what makes the system event-driven rather than merely
//! time-driven — a job can be attached to any event name on the kernel bus, including
//! events from subsystems the scheduler knows nothing about.

use std::time::Duration;

use aik_core::Result;
use aik_core::clock::Timestamp;
use aik_core::id::EventName;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;

aik_core::string_id! {
    /// Names a scheduled job. Stable, so a job can be replaced or cancelled across restarts.
    pub JobId
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

/// A job to be scheduled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The job's stable name.
    pub id: JobId,
    /// When it runs.
    pub trigger: Trigger,
    /// Which handler runs it, as registered in the kernel registry.
    pub handler: aik_core::ComponentId,
    /// Handler-specific data.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// Whether the job survives a restart. Implementations without persistence should
    /// reject `true` rather than quietly forget.
    #[serde(default)]
    pub persistent: bool,
}

/// A job as the scheduler currently sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJob {
    /// What was scheduled.
    pub spec: JobSpec,
    /// When it will next run, if that is knowable.
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
    async fn schedule(&self, spec: JobSpec) -> Result<()>;

    /// Cancels a job, returning whether it existed.
    async fn cancel(&self, id: &JobId) -> Result<bool>;

    /// Lists the scheduled jobs.
    async fn list(&self) -> Result<Vec<ScheduledJob>>;
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
}
