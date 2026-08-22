//! What the persistent scheduler adds over the volatile one, and what it must not lose while
//! adding it.
//!
//! The behavioural suite in `behavior.rs` already runs against both wirings. These are the
//! assertions that only mean something on disk: that a restart changes nothing it should not,
//! that a firing interrupted by one is not repeated, that a schedule which slept through a
//! backlog resumes rather than stampedes, and that a database somebody has edited underneath
//! us is reported rather than read as though it were merely empty.
//!
//! This mirrors `aik-memory`'s and `aik-context`'s `persistence.rs` deliberately: three
//! durable subsystems share one file and one set of failure modes, and a gap in one suite is
//! a gap in the guarantee all three make.

use std::time::Duration;

use aik_api::permission::PrincipalId;
use aik_api::scheduler::{
    JobCancelled, JobCompleted, JobId, JobSkipped, JobSpec, JobStarted, SkipReason, Trigger,
};
use aik_core::ErrorKind;
use aik_core::clock::{Clock, Timestamp};
use aik_store::Db;
use aik_store::redb::TableDefinition;

mod support;
use support::{Backend, Fixture, TestHandler, advance, anonymous, expect, expect_none, user};

/// The schedule table, as `persistent.rs` defines it. Redeclared here rather than exported,
/// because the layout is the store's own business — a test that reaches into it is
/// deliberately going behind the API, and should look like it.
const JOBS: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("sched.jobs");

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

fn durable(id: &str, trigger: Trigger) -> JobSpec {
    JobSpec::new(id, trigger, HANDLER).persistent(true)
}

async fn fixture(handler: TestHandler) -> Fixture {
    Fixture::open(Backend::Redb)
        .with_handler(HANDLER, handler)
        .start()
        .await
}

#[tokio::test(start_paused = true)]
async fn a_persistent_job_survives_a_restart_intact() {
    let mut f = fixture(TestHandler::new()).await;
    let spec = durable("nightly", every(600))
        .with_payload(serde_json::json!({ "channel": "email" }))
        .with_retry(aik_api::scheduler::RetryPolicy::attempts(3))
        .with_timeout(Duration::from_secs(30));
    f.scheduler()
        .schedule(spec.clone(), &user("alice"))
        .await
        .unwrap();

    let before = f.scheduler().list(&user("alice")).await.unwrap();
    f.restart().await;
    let after_restart = f.scheduler().list(&user("alice")).await.unwrap();

    assert_eq!(after_restart.len(), 1);
    assert_eq!(after_restart[0].spec, spec, "every field comes back");
    assert_eq!(after_restart[0].owner, PrincipalId::new("alice"));
    assert_eq!(after_restart[0].next_run, before[0].next_run);
    assert_eq!(after_restart[0].last_run, before[0].last_run);
}

#[tokio::test(start_paused = true)]
async fn a_volatile_job_does_not_survive_a_restart() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(JobSpec::new("ephemeral", every(600), HANDLER), &anonymous())
        .await
        .unwrap();
    f.scheduler()
        .schedule(durable("durable", every(600)), &anonymous())
        .await
        .unwrap();

    f.restart().await;

    let listed: Vec<JobId> = f
        .scheduler()
        .list(&anonymous())
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.spec.id)
        .collect();
    assert_eq!(
        listed,
        vec![JobId::new("durable")],
        "a job that did not ask to be persistent must never be written down"
    );
}

#[tokio::test(start_paused = true)]
async fn a_persistent_job_keeps_its_cadence_across_a_restart() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("tick", every(60)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(60)).await;
    f.handler(HANDLER).wait_for_calls(1).await;
    let due_after_first = f.scheduler().list(&anonymous()).await.unwrap()[0]
        .next_run
        .expect("a periodic job knows its next run");

    f.restart().await;

    assert_eq!(
        f.scheduler().list(&anonymous()).await.unwrap()[0].next_run,
        Some(due_after_first),
        "a restart must not restart the phase of a periodic job"
    );
    advance(Duration::from_secs(60)).await;
    f.handler(HANDLER).wait_for_calls(2).await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_persistent_job_is_durable() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("nightly", every(600)), &anonymous())
        .await
        .unwrap();
    assert!(
        f.scheduler()
            .cancel(&JobId::new("nightly"), &anonymous())
            .await
            .unwrap()
    );

    f.restart().await;

    assert!(
        f.scheduler().list(&anonymous()).await.unwrap().is_empty(),
        "a cancellation the caller was told succeeded must not be undone by a restart"
    );
}

#[tokio::test(start_paused = true)]
async fn replacing_a_persistent_job_with_a_volatile_one_takes_the_row_with_it() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("job", every(600)), &anonymous())
        .await
        .unwrap();
    f.scheduler()
        .schedule(JobSpec::new("job", every(600), HANDLER), &anonymous())
        .await
        .unwrap();

    f.restart().await;

    assert!(
        f.scheduler().list(&anonymous()).await.unwrap().is_empty(),
        "the replaced row would otherwise resurrect a job the caller replaced"
    );
}

#[tokio::test(start_paused = true)]
async fn a_firing_interrupted_by_a_restart_is_not_repeated() {
    let mut f = fixture(TestHandler::new().holding()).await;
    let mut cancelled = f.watch::<JobCancelled>();
    f.scheduler()
        .schedule(durable("once", after(10)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(10)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // Stopping mid-handler is the closest a test gets to losing the process: the firing was
    // claimed, the handler was called, and nothing said it finished.
    f.restart().await;
    expect(&mut cancelled).await;

    advance(Duration::from_secs(600)).await;
    let mut started = f.watch::<JobStarted>();
    expect_none(&mut started).await;
    assert_eq!(
        f.handler(HANDLER).count(),
        1,
        "the schedule is advanced before the handler runs, so a lost firing is lost rather \
         than repeated"
    );
    assert!(f.scheduler().list(&anonymous()).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_firing_missed_within_the_catch_up_window_runs_once() {
    let mut f = Fixture::open(Backend::Redb)
        .with_handler(HANDLER, TestHandler::new())
        .with_catch_up_window(Duration::from_secs(3_600))
        .start()
        .await;
    f.scheduler()
        .schedule(durable("reminder", after(60)), &anonymous())
        .await
        .unwrap();

    f.close().await;
    // Off for ten minutes, over a firing that was due one minute in.
    advance(Duration::from_secs(600)).await;
    f.restart().await;

    f.handler(HANDLER).wait_for_calls(1).await;
    advance(Duration::from_secs(3_600)).await;
    assert_eq!(
        f.handler(HANDLER).count(),
        1,
        "one missed occurrence is one firing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_firing_missed_beyond_the_catch_up_window_is_reported_and_dropped() {
    let mut f = Fixture::open(Backend::Redb)
        .with_handler(HANDLER, TestHandler::new())
        .with_catch_up_window(Duration::from_secs(60))
        .start()
        .await;
    let mut skipped = f.watch::<JobSkipped>();
    f.scheduler()
        .schedule(durable("reminder", after(60)), &anonymous())
        .await
        .unwrap();

    f.close().await;
    advance(Duration::from_secs(86_400)).await;
    f.restart().await;

    let skip = expect(&mut skipped).await;
    assert_eq!(skip.reason, SkipReason::Missed);
    assert_eq!(skip.event.job, JobId::new("reminder"));

    assert_eq!(f.handler(HANDLER).count(), 0);
    assert!(
        f.scheduler().list(&anonymous()).await.unwrap().is_empty(),
        "a one-shot that can never usefully run again is not kept as a tombstone"
    );
}

#[tokio::test(start_paused = true)]
async fn a_periodic_job_that_slept_through_a_backlog_resumes_rather_than_stampedes() {
    let mut f = Fixture::open(Backend::Redb)
        .with_handler(HANDLER, TestHandler::new())
        .with_catch_up_window(Duration::from_secs(60))
        .start()
        .await;
    let mut skipped = f.watch::<JobSkipped>();
    f.scheduler()
        .schedule(durable("tick", every(60)), &anonymous())
        .await
        .unwrap();

    f.close().await;
    // A day off: fourteen hundred occurrences elapsed.
    advance(Duration::from_secs(86_400)).await;
    let restarted_at = f.clock().now();
    f.restart().await;

    let skip = expect(&mut skipped).await;
    assert_eq!(skip.reason, SkipReason::Missed);

    let next = f.scheduler().list(&anonymous()).await.unwrap()[0]
        .next_run
        .expect("the job is still periodic");
    assert!(next > restarted_at, "the schedule resumed ahead of now");
    assert!(
        next.saturating_since(restarted_at) <= Duration::from_secs(60),
        "and within one interval of it, so the cadence is kept rather than restarted"
    );

    advance(Duration::from_secs(60)).await;
    f.handler(HANDLER).wait_for_calls(1).await;
    assert_eq!(
        f.handler(HANDLER).count(),
        1,
        "a backlog is reported once, not delivered"
    );
}

#[tokio::test(start_paused = true)]
async fn a_firing_resumed_late_keeps_the_cadence_it_had_rather_than_restarting_it() {
    let mut f = Fixture::open(Backend::Redb)
        .with_handler(HANDLER, TestHandler::new())
        .with_catch_up_window(Duration::from_secs(3_600))
        .start()
        .await;
    let scheduled_at = f.clock().now();
    f.scheduler()
        .schedule(durable("tick", every(60)), &anonymous())
        .await
        .unwrap();
    let first_due = scheduled_at.saturating_add(Duration::from_secs(60));

    f.close().await;
    // Back after two and a half minutes: the occurrence due at +60s is overdue, and the one
    // at +120s went by entirely.
    advance(Duration::from_secs(150)).await;
    f.restart().await;
    f.handler(HANDLER).wait_for_calls(1).await;

    let listed = &f.scheduler().list(&anonymous()).await.unwrap()[0];
    assert_eq!(
        listed.last_run,
        Some(first_due),
        "the firing is for the occurrence that was due, not for the moment it was reached"
    );
    assert_eq!(
        listed.next_run,
        Some(scheduled_at.saturating_add(Duration::from_secs(180))),
        "a job reached late resumes the cadence it always had; anchoring on the late arrival \
         instead would shift every future occurrence by however long the delay was"
    );
}

#[tokio::test(start_paused = true)]
async fn a_job_still_runs_for_its_owner_after_a_restart() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("job", after(60)), &user("alice"))
        .await
        .unwrap();

    f.restart().await;
    advance(Duration::from_secs(60)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    let principal = f.handler(HANDLER).calls()[0]
        .principal
        .clone()
        .expect("a firing is never anonymous");
    assert!(
        principal.may_act_for(&PrincipalId::new("alice")),
        "the authority a job carries is part of what persistence has to preserve"
    );
}

#[tokio::test(start_paused = true)]
async fn a_row_that_cannot_be_decoded_fails_the_restart_rather_than_vanishing() {
    let mut f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("nightly", every(600)), &anonymous())
        .await
        .unwrap();
    f.close().await;

    // Something else has been at the file.
    {
        let db = Db::open(f.path()).expect("the database opens");
        let transaction = db.database().begin_write().unwrap();
        {
            let mut table = transaction.open_table(JOBS).unwrap();
            table
                .insert("nightly", b"not json at all".as_slice())
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    let error = f
        .try_restart()
        .await
        .expect_err("a schedule that cannot be read is not an empty schedule");
    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(error.to_string().contains("nightly"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn the_schedule_shares_one_database_with_another_durable_subsystem() {
    use aik_api::memory::{MemoryRecord, MemoryStore};

    let f = fixture(TestHandler::new()).await;
    f.scheduler()
        .schedule(durable("nightly", every(600)), &user("alice"))
        .await
        .unwrap();

    // A second durable subsystem, over the *same handle* -- redb hands the file to one
    // process, so sharing a database means sharing the `Arc<Db>` the kernel registry holds.
    let memories = aik_memory::RedbMemoryStore::new(f.db()).expect("the memory tables exist");
    let record = MemoryRecord::new("fact", serde_json::json!({ "n": 1 }), Timestamp::EPOCH);
    let id = record.id;
    memories.put(record, &user("alice")).await.unwrap();

    assert!(memories.get(&id, &user("alice")).await.unwrap().is_some());
    assert_eq!(
        f.scheduler().list(&user("alice")).await.unwrap().len(),
        1,
        "two subsystems in one file do not disturb each other"
    );
}

#[tokio::test(start_paused = true)]
async fn a_completed_one_shot_is_removed_from_the_database_not_merely_from_memory() {
    let mut f = fixture(TestHandler::new()).await;
    let mut completed = f.watch::<JobCompleted>();
    f.scheduler()
        .schedule(durable("once", after(10)), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(10)).await;
    expect(&mut completed).await;

    f.restart().await;
    assert!(f.scheduler().list(&anonymous()).await.unwrap().is_empty());
    assert_eq!(f.handler(HANDLER).count(), 1);
}
