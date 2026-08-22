//! The [`Scheduler`] contract, run once against both wirings.
//!
//! A persistent scheduler that fired on a different schedule, retried differently or ran jobs
//! as a different principal from the volatile one would be a correctness regression delivered
//! as a durability improvement. Writing every assertion here once and running it against both
//! is what keeps that impossible by construction rather than by discipline.

use std::time::Duration;

use aik_api::permission::{PrincipalId, PrincipalKind};
use aik_api::scheduler::{
    JobCancelled, JobCompleted, JobFailed, JobId, JobSkipped, JobSpec, JobStarted, RetryPolicy,
    SkipReason, Trigger,
};
use aik_core::ErrorKind;
use aik_core::clock::Clock;
use aik_core::event::Event;
use aik_core::id::EventName;
use serde::{Deserialize, Serialize};
use serde_json::json;

mod support;
use support::{
    Backend, Fixture, TestHandler, advance, anonymous, expect, expect_none, until, user,
};

crate::both_backends!(
    a_due_job_fires,
    a_one_shot_job_fires_once_and_leaves_the_schedule,
    an_after_trigger_is_measured_from_scheduling_time,
    an_at_trigger_already_in_the_past_fires_immediately,
    a_periodic_job_keeps_firing_on_its_cadence,
    a_periodic_job_does_not_fire_before_its_first_interval,
    scheduling_an_existing_id_replaces_it,
    cancel_reports_whether_the_job_existed,
    a_cancelled_job_never_fires,
    listing_reports_where_the_schedule_has_got_to,
    an_event_triggered_job_fires_when_its_event_is_published,
    an_event_triggered_job_has_no_next_run,
    an_event_the_job_does_not_name_does_not_fire_it,
    the_scheduler_does_not_trigger_jobs_on_its_own_events,
    cron_is_refused_at_scheduling_time,
    an_interval_that_cannot_advance_is_refused,
    persistence_is_accepted_only_where_it_can_be_honoured,
    an_overlapping_firing_is_skipped_rather_than_queued,
    a_skipped_firing_still_advances_the_schedule,
    a_failing_firing_is_retried_up_to_its_limit,
    a_failing_firing_without_a_retry_policy_is_not_retried,
    a_retry_that_succeeds_ends_the_firing,
    every_attempt_of_one_firing_shares_its_identity,
    a_firing_that_overruns_its_deadline_fails_with_a_timeout,
    a_deadline_reaches_the_handler_that_has_to_honour_it,
    a_firing_runs_as_the_system_acting_for_its_owner,
    cancelling_a_job_cancels_the_firing_in_flight,
    cancelling_a_job_abandons_a_pending_retry,
    replacing_a_job_leaves_the_firing_in_flight_alone,
    a_handler_that_ignores_cancellation_still_reports_its_outcome,
    a_job_naming_an_unregistered_handler_is_reported,
    no_event_carries_the_job_payload,
    scheduling_after_shutdown_is_refused,
);

const HANDLER: &str = "jobs.test";

fn every(seconds: u64) -> Trigger {
    Trigger::Every {
        interval: Duration::from_secs(seconds),
    }
}

fn after(seconds: u64) -> Trigger {
    Trigger::After {
        delay: Duration::from_secs(seconds),
    }
}

fn spec(id: &str, trigger: Trigger) -> JobSpec {
    JobSpec::new(id, trigger, HANDLER)
}

async fn fixture(backend: Backend, handler: TestHandler) -> Fixture {
    Fixture::open(backend)
        .with_handler(HANDLER, handler)
        .start()
        .await
}

async fn a_due_job_fires(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(60)).await;

    f.handler(HANDLER).wait_for_calls(1).await;
    assert_eq!(f.handler(HANDLER).calls()[0].job, JobId::new("job"));
}

async fn a_one_shot_job_fires_once_and_leaves_the_schedule(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut completed = f.watch::<JobCompleted>();
    f.scheduler()
        .schedule(spec("once", after(10)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(10)).await;
    expect(&mut completed).await;

    assert!(
        f.scheduler().list(&anonymous()).await.unwrap().is_empty(),
        "a job that can never run again is not a job the schedule still holds"
    );
    // And it is gone rather than merely dormant: cancelling it reports nothing to cancel.
    assert!(
        !f.scheduler()
            .cancel(&JobId::new("once"), &anonymous())
            .await
            .unwrap()
    );
}

async fn an_after_trigger_is_measured_from_scheduling_time(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let scheduled_at = f.clock().now();
    f.scheduler()
        .schedule(spec("job", after(30)), &anonymous())
        .await
        .unwrap();

    let listed = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(
        listed.next_run,
        Some(scheduled_at.saturating_add(Duration::from_secs(30)))
    );
}

async fn an_at_trigger_already_in_the_past_fires_immediately(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let long_ago = aik_core::clock::Timestamp::from_millis(f.clock().now().as_millis() - 5_000);
    f.scheduler()
        .schedule(
            spec(
                "late",
                Trigger::At {
                    timestamp: long_ago,
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap();

    f.handler(HANDLER).wait_for_calls(1).await;
}

async fn a_periodic_job_keeps_firing_on_its_cadence(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("tick", every(10)), &anonymous())
        .await
        .unwrap();

    for expected in 1..=3 {
        advance(Duration::from_secs(10)).await;
        f.handler(HANDLER).wait_for_calls(expected).await;
    }
    assert_eq!(f.handler(HANDLER).count(), 3);
}

async fn a_periodic_job_does_not_fire_before_its_first_interval(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut started = f.watch::<JobStarted>();
    f.scheduler()
        .schedule(spec("tick", every(60)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(59)).await;

    expect_none(&mut started).await;
    assert_eq!(f.handler(HANDLER).count(), 0);
}

async fn scheduling_an_existing_id_replaces_it(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(10)), &anonymous())
        .await
        .unwrap();
    f.scheduler()
        .schedule(spec("job", after(30)), &anonymous())
        .await
        .unwrap();

    let listed = f.scheduler().list(&anonymous()).await.unwrap();
    assert_eq!(listed.len(), 1, "one id is one job");

    // The replaced trigger is gone, not merely shadowed.
    advance(Duration::from_secs(10)).await;
    assert_eq!(f.handler(HANDLER).count(), 0);
    advance(Duration::from_secs(20)).await;
    f.handler(HANDLER).wait_for_calls(1).await;
    assert_eq!(f.handler(HANDLER).count(), 1);
}

async fn cancel_reports_whether_the_job_existed(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let id = JobId::new("job");

    assert!(!f.scheduler().cancel(&id, &anonymous()).await.unwrap());
    f.scheduler()
        .schedule(spec("job", after(10)), &anonymous())
        .await
        .unwrap();
    assert!(f.scheduler().cancel(&id, &anonymous()).await.unwrap());
    assert!(!f.scheduler().cancel(&id, &anonymous()).await.unwrap());
}

async fn a_cancelled_job_never_fires(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut started = f.watch::<JobStarted>();
    f.scheduler()
        .schedule(spec("job", every(10)), &anonymous())
        .await
        .unwrap();
    f.scheduler()
        .cancel(&JobId::new("job"), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(100)).await;

    expect_none(&mut started).await;
    assert_eq!(f.handler(HANDLER).count(), 0);
}

async fn listing_reports_where_the_schedule_has_got_to(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("tick", every(10)), &anonymous())
        .await
        .unwrap();

    let before = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(before.last_run, None);
    let first_due = before.next_run.expect("a periodic job knows its next run");

    advance(Duration::from_secs(10)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    let after_firing = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(after_firing.last_run, Some(first_due));
    assert_eq!(
        after_firing.next_run,
        Some(first_due.saturating_add(Duration::from_secs(10))),
        "the cadence is anchored on the occurrence, not on when the handler ran"
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Poked;

impl Event for Poked {
    const NAME: &'static str = "test.poked";
}

async fn an_event_triggered_job_fires_when_its_event_is_published(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(
            spec(
                "reactive",
                Trigger::OnEvent {
                    event: Poked::event_name(),
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap();

    // The scheduler subscribes to the firehose only once a job needs it, so the publication
    // has to lose a race to the driver noticing -- which is what this retry loop tolerates.
    until("an event-triggered job to run", async || {
        if f.handler(HANDLER).count() > 0 {
            return true;
        }
        f.events().publish(Poked);
        false
    })
    .await;
}

async fn an_event_triggered_job_has_no_next_run(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(
            spec(
                "reactive",
                Trigger::OnEvent {
                    event: Poked::event_name(),
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap();

    let listed = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(
        listed.next_run, None,
        "nobody can say when an event will next happen"
    );
}

async fn an_event_the_job_does_not_name_does_not_fire_it(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut started = f.watch::<JobStarted>();
    f.scheduler()
        .schedule(
            spec(
                "reactive",
                Trigger::OnEvent {
                    event: EventName::new("test.something_else"),
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap();

    for _ in 0..8 {
        f.events().publish(Poked);
        tokio::task::yield_now().await;
    }

    expect_none(&mut started).await;
}

async fn the_scheduler_does_not_trigger_jobs_on_its_own_events(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut started = f.watch::<JobStarted>();

    // A job triggered by the event its own completion publishes: the loop this guards against.
    f.scheduler()
        .schedule(
            spec(
                "ouroboros",
                Trigger::OnEvent {
                    event: JobCompleted::event_name(),
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap();
    // Something else that will actually complete, and so publish the triggering event.
    f.scheduler()
        .schedule(spec("real", after(1)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    let first = expect(&mut started).await;
    assert_eq!(first.event.job, JobId::new("real"));

    expect_none(&mut started).await;
    assert_eq!(
        f.handler(HANDLER).count(),
        1,
        "the scheduler's own events must not feed back into it"
    );
}

async fn cron_is_refused_at_scheduling_time(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let error = f
        .scheduler()
        .schedule(
            spec(
                "nightly",
                Trigger::Cron {
                    expression: "0 3 * * *".into(),
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(f.scheduler().list(&anonymous()).await.unwrap().is_empty());
}

async fn an_interval_that_cannot_advance_is_refused(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let error = f
        .scheduler()
        .schedule(
            spec(
                "spin",
                Trigger::Every {
                    interval: Duration::ZERO,
                },
            ),
            &anonymous(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(f.scheduler().list(&anonymous()).await.unwrap().is_empty());
}

async fn persistence_is_accepted_only_where_it_can_be_honoured(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let result = f
        .scheduler()
        .schedule(spec("durable", after(10)).persistent(true), &anonymous())
        .await;

    if backend.persists() {
        result.expect("a scheduler with a store keeps persistent jobs");
        assert_eq!(f.scheduler().list(&anonymous()).await.unwrap().len(), 1);
    } else {
        let error = result.expect_err("a volatile scheduler must not quietly forget a job");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            f.scheduler().list(&anonymous()).await.unwrap().is_empty(),
            "a refused job is not half-scheduled"
        );
    }
}

async fn an_overlapping_firing_is_skipped_rather_than_queued(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    let mut skipped = f.watch::<JobSkipped>();
    f.scheduler()
        .schedule(spec("slow", every(10)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(10)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // The second occurrence comes due while the first is still in the handler.
    advance(Duration::from_secs(10)).await;
    let skip = expect(&mut skipped).await;
    assert_eq!(skip.reason, SkipReason::AlreadyRunning);
    assert_eq!(skip.event.job, JobId::new("slow"));

    assert_eq!(f.handler(HANDLER).count(), 1);
    assert_eq!(
        f.handler(HANDLER).peak_in_flight(),
        1,
        "one firing of a job at a time, always"
    );

    f.handler(HANDLER).release();
}

async fn a_skipped_firing_still_advances_the_schedule(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    f.scheduler()
        .schedule(spec("slow", every(10)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(10)).await;
    f.handler(HANDLER).wait_for_calls(1).await;
    let first_due = f.scheduler().list(&anonymous()).await.unwrap()[0]
        .last_run
        .expect("the first firing was recorded");

    advance(Duration::from_secs(10)).await;
    until(
        "the skipped occurrence to move the schedule on",
        async || f.scheduler().list(&anonymous()).await.unwrap()[0].last_run != Some(first_due),
    )
    .await;

    let listed = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(
        listed.next_run,
        Some(first_due.saturating_add(Duration::from_secs(20))),
        "a job that falls behind must not accumulate a backlog to deliver at once"
    );

    f.handler(HANDLER).release();
}

async fn a_failing_firing_is_retried_up_to_its_limit(backend: Backend) {
    let f = fixture(backend, TestHandler::new().always_failing()).await;
    let mut failures = f.watch::<JobFailed>();
    f.scheduler()
        .schedule(
            spec("flaky", after(1)).with_retry(
                RetryPolicy::attempts(2)
                    .with_backoff(Duration::from_secs(1))
                    .with_max_backoff(Duration::from_secs(4)),
            ),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let first = expect(&mut failures).await;
    assert_eq!((first.attempt, first.will_retry), (0, true));
    let second = expect(&mut failures).await;
    assert_eq!((second.attempt, second.will_retry), (1, true));
    let last = expect(&mut failures).await;
    assert_eq!(
        (last.attempt, last.will_retry),
        (2, false),
        "two retries is two retries, and the last failure says so"
    );

    expect_none(&mut failures).await;
    assert_eq!(f.handler(HANDLER).count(), 3);
}

async fn a_failing_firing_without_a_retry_policy_is_not_retried(backend: Backend) {
    let f = fixture(backend, TestHandler::new().always_failing()).await;
    let mut failures = f.watch::<JobFailed>();
    f.scheduler()
        .schedule(spec("flaky", after(1)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let only = expect(&mut failures).await;
    assert_eq!((only.attempt, only.will_retry), (0, false));
    expect_none(&mut failures).await;
    assert_eq!(f.handler(HANDLER).count(), 1);
}

async fn a_retry_that_succeeds_ends_the_firing(backend: Backend) {
    let f = fixture(backend, TestHandler::new().failing_first(1)).await;
    let mut completed = f.watch::<JobCompleted>();
    f.scheduler()
        .schedule(
            spec("flaky", after(1))
                .with_retry(RetryPolicy::attempts(5).with_backoff(Duration::from_secs(1))),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let success = expect(&mut completed).await;
    assert_eq!(success.attempt, 1, "the second attempt is the one that won");
    assert_eq!(
        f.handler(HANDLER).count(),
        2,
        "a firing stops retrying the moment it succeeds"
    );
}

async fn every_attempt_of_one_firing_shares_its_identity(backend: Backend) {
    let f = fixture(backend, TestHandler::new().failing_first(1)).await;
    let mut started = f.watch::<JobStarted>();
    f.scheduler()
        .schedule(
            spec("flaky", after(1))
                .with_retry(RetryPolicy::attempts(1).with_backoff(Duration::from_secs(1))),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let first = expect(&mut started).await;
    let second = expect(&mut started).await;
    assert_eq!(
        (first.event.run, first.event.correlation),
        (second.event.run, second.event.correlation),
        "a retry is the same firing, so an observer counts occurrences and not attempts"
    );
    assert_eq!((first.attempt, second.attempt), (0, 1));
    assert_eq!(
        first.scheduled_for, second.scheduled_for,
        "both attempts are for the occurrence that was due"
    );

    let calls = f.handler(HANDLER).calls();
    assert_eq!(calls[0].correlation, calls[1].correlation);
}

async fn a_firing_that_overruns_its_deadline_fails_with_a_timeout(backend: Backend) {
    let f = fixture(
        backend,
        TestHandler::new().sleeping(Duration::from_secs(600)),
    )
    .await;
    let mut failures = f.watch::<JobFailed>();
    f.scheduler()
        .schedule(
            spec("slow", after(1)).with_timeout(Duration::from_secs(5)),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let failure = expect(&mut failures).await;
    assert_eq!(failure.kind, "timeout");
    assert!(!failure.will_retry);
}

async fn a_deadline_reaches_the_handler_that_has_to_honour_it(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut started = f.watch::<JobStarted>();
    f.scheduler()
        .schedule(
            spec("bounded", after(1)).with_timeout(Duration::from_secs(30)),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    let start = expect(&mut started).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    assert_eq!(
        f.handler(HANDLER).calls()[0].deadline,
        Some(
            start
                .event
                .timestamp
                .saturating_add(Duration::from_secs(30))
        ),
        "a deadline the handler cannot see is a deadline only the scheduler enforces"
    );
}

async fn a_firing_runs_as_the_system_acting_for_its_owner(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(1)), &user("alice"))
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    let principal = f.handler(HANDLER).calls()[0]
        .principal
        .clone()
        .expect("a firing is never anonymous");
    assert_eq!(principal.kind, PrincipalKind::System);
    assert_eq!(principal.id, PrincipalId::new(aik_scheduler::RUN_PRINCIPAL));
    assert!(principal.may_act_for(&PrincipalId::new("alice")));
    assert_ne!(
        principal.id,
        PrincipalId::new("alice"),
        "a scheduled job acts for its owner rather than becoming them"
    );
}

async fn cancelling_a_job_cancels_the_firing_in_flight(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    let mut cancelled = f.watch::<JobCancelled>();
    f.scheduler()
        .schedule(spec("slow", after(1)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // A one-shot job has already left the schedule by now -- that is what makes it fire once
    // -- so this also pins down that cancelling reaches the firing and not merely the entry.
    assert!(
        f.scheduler()
            .cancel(&JobId::new("slow"), &anonymous())
            .await
            .unwrap(),
        "a job the caller can watch running is a job there is something to cancel"
    );

    let event = expect(&mut cancelled).await;
    assert_eq!(event.event.job, JobId::new("slow"));
    assert!(
        f.handler(HANDLER).observed_cancellation(),
        "cancellation reaches the handler's context, which is the only way it can stop"
    );
}

async fn cancelling_a_job_abandons_a_pending_retry(backend: Backend) {
    let f = fixture(backend, TestHandler::new().always_failing()).await;
    let mut failures = f.watch::<JobFailed>();
    f.scheduler()
        .schedule(
            spec("flaky", after(1))
                .with_retry(RetryPolicy::attempts(5).with_backoff(Duration::from_secs(3_600))),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    let first = expect(&mut failures).await;
    assert!(first.will_retry, "the firing is waiting to try again");

    f.scheduler()
        .cancel(&JobId::new("flaky"), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(7_200)).await;
    expect_none(&mut failures).await;
    assert_eq!(
        f.handler(HANDLER).count(),
        1,
        "a cancelled job does not come back after its backoff"
    );
}

async fn replacing_a_job_leaves_the_firing_in_flight_alone(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    let mut cancelled = f.watch::<JobCancelled>();
    let mut completed = f.watch::<JobCompleted>();
    f.scheduler()
        .schedule(spec("job", after(1)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // A handler reprogramming its own schedule must not thereby kill itself.
    f.scheduler()
        .schedule(spec("job", every(600)), &anonymous())
        .await
        .unwrap();

    expect_none(&mut cancelled).await;
    assert!(!f.handler(HANDLER).observed_cancellation());

    f.handler(HANDLER).release();
    expect(&mut completed).await;
}

async fn a_handler_that_ignores_cancellation_still_reports_its_outcome(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding().deaf_to_cancellation()).await;
    let mut completed = f.watch::<JobCompleted>();
    f.scheduler()
        .schedule(spec("stubborn", after(1)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    f.scheduler()
        .cancel(&JobId::new("stubborn"), &anonymous())
        .await
        .unwrap();
    f.handler(HANDLER).release();

    // Nothing was aborted, so the run finished on its own terms and said so.
    let done = expect(&mut completed).await;
    assert_eq!(done.event.job, JobId::new("stubborn"));
}

async fn a_job_naming_an_unregistered_handler_is_reported(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    let mut failures = f.watch::<JobFailed>();
    f.scheduler()
        .schedule(
            JobSpec::new("orphan", after(1), "jobs.absent"),
            &anonymous(),
        )
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;

    let failure = expect(&mut failures).await;
    assert_eq!(failure.kind, "notfound");
    assert!(!failure.will_retry, "asking again cannot change the answer");
    assert!(failure.error.contains("jobs.absent"), "{}", failure.error);
}

async fn no_event_carries_the_job_payload(backend: Backend) {
    const SECRET: &str = "swordfish";
    let f = fixture(backend, TestHandler::new().always_failing()).await;
    let mut firehose = f.events().subscribe_any();

    f.scheduler()
        .schedule(
            spec("job", after(1)).with_payload(json!({ "token": SECRET })),
            &user("alice"),
        )
        .await
        .unwrap();
    advance(Duration::from_secs(1)).await;

    f.handler(HANDLER).wait_for_calls(1).await;
    assert_eq!(
        f.handler(HANDLER).calls()[0].payload["token"],
        json!(SECRET),
        "the handler does get the payload; it is the events that must not"
    );

    let mut seen = 0;
    while let Some(Ok(envelope)) = firehose.try_recv() {
        seen += 1;
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(
            !json.contains(SECRET),
            "`{}` leaked the payload: {json}",
            envelope.metadata.name
        );
    }
    assert!(seen >= 2, "the firing published a start and an outcome");
}

async fn scheduling_after_shutdown_is_refused(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.concrete().stop();

    let error = f
        .scheduler()
        .schedule(spec("job", after(1)), &anonymous())
        .await
        .unwrap_err();

    assert_eq!(
        error.kind(),
        ErrorKind::Cancelled,
        "accepting a job nothing is left to run would be the quiet failure this avoids"
    );
}
