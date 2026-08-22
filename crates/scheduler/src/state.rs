//! When a job runs: the arithmetic behind every [`Trigger`], in one place.
//!
//! Nothing here touches the clock, the database or the event bus. A trigger plus a timestamp
//! goes in, the next timestamp comes out, and that is the whole of it — which is what makes
//! the rules that are genuinely hard to get right (what happens to a schedule while the
//! machine is off, whether a periodic job drifts, whether a backlog fires all at once)
//! testable without a running kernel.

use std::time::Duration;

use aik_api::permission::Principal;
use aik_api::scheduler::{JobSpec, ScheduledJob, Trigger};
use aik_core::clock::Timestamp;
use aik_core::{Error, Result};

/// One job, as the scheduler holds it: what was asked for, plus where the schedule has got to.
///
/// Public because it is what a [`JobStore`](crate::JobStore) reads and writes; the arithmetic
/// that moves it forward is not, because deciding when a job runs is the scheduler's alone.
#[derive(Debug, Clone)]
pub struct JobState {
    /// What was scheduled.
    pub spec: JobSpec,
    /// The principal that scheduled it, and whose authority its firings carry.
    ///
    /// The whole principal rather than only its id: the kind is what a policy engine reads to
    /// tell a user's job from an agent's, and reconstructing it from the id later would be
    /// guessing.
    pub owner: Principal,
    /// When the next firing is due, if that is knowable.
    pub next_run: Option<Timestamp>,
    /// When the job last fired.
    pub last_run: Option<Timestamp>,
}

impl JobState {
    /// The public view of this job.
    pub fn to_scheduled(&self) -> ScheduledJob {
        ScheduledJob {
            spec: self.spec.clone(),
            owner: self.owner.id.clone(),
            next_run: self.next_run,
            last_run: self.last_run,
        }
    }

    /// Whether this job is waiting for an event rather than for a time.
    pub fn is_event_driven(&self) -> bool {
        matches!(self.spec.trigger, Trigger::OnEvent { .. })
    }

    /// The event this job waits for, if it waits for one.
    pub fn awaited_event(&self) -> Option<&aik_core::id::EventName> {
        match &self.spec.trigger {
            Trigger::OnEvent { event } => Some(event),
            _ => None,
        }
    }
}

/// Refuses a specification this scheduler cannot honour, before anything is written.
///
/// `persistence` is whether the scheduler has somewhere durable to put a job. A specification
/// asking for [`persistent`](JobSpec::persistent) where there is not is refused rather than
/// accepted and quietly forgotten, which is what the contract asks for and what stops a
/// deployment from believing its 3am job exists.
pub(crate) fn validate(spec: &JobSpec, persistence: bool) -> Result<()> {
    if spec.persistent && !persistence {
        return Err(Error::Unsupported(format!(
            "job `{}` asks to be persistent, but this scheduler keeps its schedule in memory \
             only; wire the persistent scheduler component instead",
            spec.id
        )));
    }

    match &spec.trigger {
        Trigger::Every { interval } if interval.as_millis() == 0 => {
            Err(Error::InvalidArgument(format!(
                "job `{}` asks to repeat every {interval:?}, which is below the one-millisecond \
                 resolution the kernel clock keeps and would fire without ever advancing",
                spec.id
            )))
        }
        // Rejected at scheduling time rather than at fire time, which is what the contract
        // asks of a scheduler that cannot parse an expression -- and this one cannot parse
        // any, because no cron dialect is defined here. Defining one means choosing between
        // several incompatible conventions and carrying a parser for it; `Every` covers the
        // periodic case, and the choice is better made when something actually needs a
        // calendar rather than an interval.
        Trigger::Cron { expression } => Err(Error::Unsupported(format!(
            "job `{}` uses the cron expression `{expression}`, but this scheduler defines no \
             cron dialect; use an `every` trigger",
            spec.id
        ))),
        _ => Ok(()),
    }
}

/// When a newly scheduled job first fires, or `None` if that depends on an event.
///
/// A [`Trigger::At`] already in the past fires immediately and once, rather than being
/// refused: the caller asked for a time, and the nearest honest answer to "do it at a moment
/// that has passed" is "do it now". [`Trigger::Every`] waits a full interval first, so
/// scheduling a job is never itself a reason for it to run.
pub(crate) fn first_run(trigger: &Trigger, now: Timestamp) -> Option<Timestamp> {
    match trigger {
        Trigger::At { timestamp } => Some(*timestamp),
        Trigger::After { delay } => Some(now.saturating_add(*delay)),
        Trigger::Every { interval } => Some(now.saturating_add(*interval)),
        // Refused by `validate` long before this is reached; `None` keeps the arithmetic
        // total rather than adding a panic that could only fire if validation were bypassed.
        Trigger::Cron { .. } => None,
        Trigger::OnEvent { .. } => None,
    }
}

/// When a job fires again, having just fired for the occurrence due at `fired_for`.
///
/// `None` means never again — a one-shot job that has now run, which the scheduler removes
/// rather than keeping as a tombstone.
///
/// A periodic job is anchored on the occurrence that was *due*, not on when the handler
/// actually got to run, so a job that runs late does not drift later every time. If several
/// occurrences went by while one firing was in progress, the ones that elapsed are gone: the
/// next firing is the next occurrence strictly after `now`, never a backlog delivered at
/// once.
pub(crate) fn next_run(
    trigger: &Trigger,
    fired_for: Timestamp,
    now: Timestamp,
) -> Option<Timestamp> {
    match trigger {
        Trigger::Every { interval } => Some(next_occurrence(fired_for, *interval, now)),
        Trigger::At { .. } | Trigger::After { .. } => None,
        Trigger::Cron { .. } | Trigger::OnEvent { .. } => None,
    }
}

/// The first multiple of `interval` after `anchor` that is strictly later than `now`.
///
/// Computed rather than stepped: a one-second job and a week of downtime is six hundred
/// thousand steps, and a loop that walks them is a startup that appears to hang.
fn next_occurrence(anchor: Timestamp, interval: Duration, now: Timestamp) -> Timestamp {
    let interval_ms = u64::try_from(interval.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let anchor_ms = anchor.as_millis();
    if anchor_ms > now.as_millis() {
        return anchor;
    }
    let elapsed = now.as_millis() - anchor_ms;
    let steps = (elapsed / interval_ms).saturating_add(1);
    Timestamp::from_millis(anchor_ms.saturating_add(steps.saturating_mul(interval_ms)))
}

/// What to do with a persisted job whose schedule may have moved on while nothing was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Nothing was missed: the job is due when it says it is due.
    Pending,
    /// One occurrence came due during the downtime and is recent enough to still be worth
    /// running. It fires once, now.
    Overdue(Timestamp),
    /// An occurrence came due during the downtime and is too old to be worth running.
    ///
    /// The scheduler reports it as skipped and moves to `next`, which is `None` for a one-shot
    /// job — it will never run, so it is removed.
    Expired {
        /// The occurrence that was missed.
        missed: Timestamp,
        /// Where the schedule goes next, if anywhere.
        next: Option<Timestamp>,
    },
}

/// Decides what a job that was due at `next_run` should do, having come back at `now`.
///
/// # Why at most one
///
/// A job that repeats every minute and a machine that was off for a day is fourteen hundred
/// missed occurrences. Running them is a stampede; running none of them silently is a system
/// that looks fine while nothing happens. Running *one* — the occurrence that is actually
/// overdue — and then resuming the normal cadence is the only one of the three that is both
/// bounded and honest, and the skipped ones are reported rather than dropped quietly.
///
/// # Why bounded by age
///
/// "Remind me at 09:00" is worth delivering at 09:02 and is not worth delivering three days
/// later; the value of a missed firing decays, and the scheduler cannot know how fast, so it
/// takes a window from configuration and errs towards not acting. Beyond the window the
/// firing is [`Recovery::Expired`] — reported, not run.
pub(crate) fn recover(state: &JobState, now: Timestamp, window: Duration) -> Recovery {
    let Some(due) = state.next_run else {
        return Recovery::Pending;
    };
    if due > now {
        return Recovery::Pending;
    }
    if now.saturating_since(due) <= window {
        return Recovery::Overdue(due);
    }
    Recovery::Expired {
        missed: due,
        next: next_run(&state.spec.trigger, due, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::PrincipalKind;
    use aik_core::ErrorKind;
    use aik_core::id::EventName;

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    fn state(trigger: Trigger, next_run: Option<Timestamp>) -> JobState {
        JobState {
            spec: JobSpec::new("job", trigger, "handler"),
            owner: Principal::new("alice", PrincipalKind::User),
            next_run,
            last_run: None,
        }
    }

    #[test]
    fn a_persistent_job_is_refused_by_a_scheduler_that_cannot_persist_it() {
        let spec = JobSpec::new("j", Trigger::At { timestamp: at(1) }, "h").persistent(true);

        let error = validate(&spec, false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains("persistent"), "{error}");

        validate(&spec, true).expect("a scheduler with a store accepts it");
    }

    #[test]
    fn cron_is_refused_at_scheduling_time_by_both_schedulers() {
        let spec = JobSpec::new(
            "j",
            Trigger::Cron {
                expression: "0 3 * * *".into(),
            },
            "h",
        );
        for persistence in [false, true] {
            let error = validate(&spec, persistence).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert!(error.to_string().contains("cron"), "{error}");
        }
    }

    #[test]
    fn an_interval_below_the_clocks_resolution_is_refused() {
        for interval in [Duration::ZERO, Duration::from_micros(999)] {
            let spec = JobSpec::new("j", Trigger::Every { interval }, "h");
            let error = validate(&spec, true).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        }
        validate(
            &JobSpec::new(
                "j",
                Trigger::Every {
                    interval: Duration::from_millis(1),
                },
                "h",
            ),
            true,
        )
        .expect("a millisecond is representable");
    }

    #[test]
    fn a_one_shot_trigger_fires_once_and_never_again() {
        let now = at(1_000);
        assert_eq!(
            first_run(&Trigger::At { timestamp: at(5) }, now),
            Some(at(5))
        );
        assert_eq!(
            first_run(
                &Trigger::After {
                    delay: Duration::from_millis(250)
                },
                now
            ),
            Some(at(1_250))
        );
        assert_eq!(
            next_run(&Trigger::At { timestamp: at(5) }, at(5), now),
            None
        );
        assert_eq!(
            next_run(
                &Trigger::After {
                    delay: Duration::from_millis(250)
                },
                at(1_250),
                now
            ),
            None
        );
    }

    #[test]
    fn a_periodic_job_waits_a_whole_interval_before_its_first_run() {
        assert_eq!(
            first_run(
                &Trigger::Every {
                    interval: Duration::from_secs(60)
                },
                at(1_000)
            ),
            Some(at(61_000))
        );
    }

    #[test]
    fn an_event_trigger_has_no_knowable_next_run() {
        let trigger = Trigger::OnEvent {
            event: EventName::new("kernel.state_changed"),
        };
        assert_eq!(first_run(&trigger, at(1_000)), None);
        assert_eq!(next_run(&trigger, at(1_000), at(2_000)), None);
    }

    #[test]
    fn a_periodic_job_that_ran_late_does_not_drift() {
        let trigger = Trigger::Every {
            interval: Duration::from_secs(10),
        };
        // Due at 10s, actually finished at 13s. The next one is at 20s, not at 23s.
        assert_eq!(
            next_run(&trigger, at(10_000), at(13_000)),
            Some(at(20_000)),
            "the cadence is anchored on when the firing was due, not on when it happened"
        );
    }

    #[test]
    fn a_periodic_job_skips_the_occurrences_it_slept_through_rather_than_queueing_them() {
        let trigger = Trigger::Every {
            interval: Duration::from_secs(10),
        };
        // Due at 10s; the handler took two minutes. The backlog is not delivered.
        assert_eq!(
            next_run(&trigger, at(10_000), at(130_000)),
            Some(at(140_000))
        );
    }

    #[test]
    fn the_next_occurrence_is_strictly_after_now_even_when_now_lands_exactly_on_one() {
        let trigger = Trigger::Every {
            interval: Duration::from_secs(10),
        };
        assert_eq!(next_run(&trigger, at(10_000), at(20_000)), Some(at(30_000)));
    }

    #[test]
    fn a_long_downtime_is_arithmetic_rather_than_a_loop() {
        let trigger = Trigger::Every {
            interval: Duration::from_millis(1),
        };
        // A week of one-millisecond occurrences. This has to return, not walk.
        let week = 7 * 24 * 60 * 60 * 1_000;
        assert_eq!(next_run(&trigger, at(0), at(week)), Some(at(week + 1)));
    }

    #[test]
    fn nothing_is_missed_when_the_next_run_is_still_ahead() {
        let job = state(
            Trigger::At {
                timestamp: at(5_000),
            },
            Some(at(5_000)),
        );
        assert_eq!(
            recover(&job, at(4_000), Duration::from_secs(3_600)),
            Recovery::Pending
        );
    }

    #[test]
    fn an_event_driven_job_has_nothing_to_catch_up_on() {
        let job = state(
            Trigger::OnEvent {
                event: EventName::new("x"),
            },
            None,
        );
        assert_eq!(
            recover(&job, at(u64::MAX), Duration::ZERO),
            Recovery::Pending
        );
    }

    #[test]
    fn a_recently_missed_firing_is_run_once() {
        let job = state(
            Trigger::At {
                timestamp: at(1_000),
            },
            Some(at(1_000)),
        );
        assert_eq!(
            recover(&job, at(1_500), Duration::from_secs(1)),
            Recovery::Overdue(at(1_000))
        );
    }

    #[test]
    fn a_firing_exactly_at_the_window_edge_still_counts_as_recent() {
        let job = state(
            Trigger::At {
                timestamp: at(1_000),
            },
            Some(at(1_000)),
        );
        assert_eq!(
            recover(&job, at(2_000), Duration::from_secs(1)),
            Recovery::Overdue(at(1_000)),
            "the boundary belongs to the side that acts, so a window of exactly N covers N"
        );
    }

    #[test]
    fn a_stale_one_shot_is_reported_and_then_has_nowhere_to_go() {
        let job = state(
            Trigger::At {
                timestamp: at(1_000),
            },
            Some(at(1_000)),
        );
        assert_eq!(
            recover(&job, at(9_000), Duration::from_secs(1)),
            Recovery::Expired {
                missed: at(1_000),
                next: None
            }
        );
    }

    #[test]
    fn a_stale_periodic_job_resumes_its_cadence_instead_of_stampeding() {
        let job = state(
            Trigger::Every {
                interval: Duration::from_secs(60),
            },
            Some(at(60_000)),
        );
        // Off for a day. One report, one future occurrence, no backlog.
        let day = 24 * 60 * 60 * 1_000;
        assert_eq!(
            recover(&job, at(day), Duration::from_secs(60)),
            Recovery::Expired {
                missed: at(60_000),
                next: Some(at(day + 60_000)),
            }
        );
    }
}
