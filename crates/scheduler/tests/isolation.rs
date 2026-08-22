//! Who may schedule, cancel and see what.
//!
//! A scheduler is where one principal's authority is most easily borrowed by another: a job
//! outlives the request that created it, runs unattended, and is named by a string anyone can
//! guess. So the rules are the same ones the memory and context stores enforce, asserted here
//! against both wirings — because an isolation rule that held only in memory would be an
//! isolation rule that stopped holding the moment the schedule was made durable.

use std::time::Duration;

use aik_api::permission::PrincipalId;
use aik_api::scheduler::{JobId, JobSpec, Trigger};
use aik_core::ErrorKind;

mod support;
use support::{Backend, Fixture, TestHandler, advance, agent_for, anonymous, user};

crate::both_backends!(
    a_job_belongs_to_the_principal_that_scheduled_it,
    a_job_scheduled_by_nobody_belongs_to_the_system,
    another_principal_cannot_cancel_a_job,
    another_principal_cannot_replace_a_job,
    a_refused_replacement_changes_nothing,
    an_agent_may_act_for_the_user_it_works_for,
    replacing_a_job_as_a_delegate_does_not_take_it_over,
    listing_returns_only_what_the_caller_may_act_for,
    another_principal_cannot_cancel_a_firing_in_flight,
    a_firing_does_not_inherit_its_owners_own_delegation,
    a_refused_cancellation_leaves_the_schedule_alone,
);

const HANDLER: &str = "jobs.test";

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

async fn a_job_belongs_to_the_principal_that_scheduled_it(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();

    assert_eq!(
        f.scheduler().list(&user("alice")).await.unwrap()[0].owner,
        PrincipalId::new("alice")
    );
}

async fn a_job_scheduled_by_nobody_belongs_to_the_system(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &anonymous())
        .await
        .unwrap();

    assert_eq!(
        f.scheduler().list(&anonymous()).await.unwrap()[0].owner,
        PrincipalId::new(aik_api::permission::Principal::SYSTEM),
        "a context with no principal is the system acting for itself, not a wildcard"
    );
}

async fn another_principal_cannot_cancel_a_job(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();

    let error = f
        .scheduler()
        .cancel(&JobId::new("job"), &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);

    assert_eq!(
        f.scheduler().list(&user("alice")).await.unwrap().len(),
        1,
        "a refused cancellation cancels nothing"
    );
}

async fn another_principal_cannot_replace_a_job(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();

    let error = f
        .scheduler()
        .schedule(spec("job", after(1)), &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Permission,
        "replacing a job must not be a way to take one over"
    );
}

async fn a_refused_replacement_changes_nothing(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();
    let before = f.scheduler().list(&user("alice")).await.unwrap();

    let _ = f
        .scheduler()
        .schedule(
            spec("job", after(1)).with_payload(serde_json::json!("mallory was here")),
            &user("mallory"),
        )
        .await;

    let after_attempt = f.scheduler().list(&user("alice")).await.unwrap();
    assert_eq!(after_attempt, before);

    // And the refusal is not merely cosmetic: the job still runs on its original schedule.
    advance(Duration::from_secs(1)).await;
    assert_eq!(f.handler(HANDLER).count(), 0);
}

async fn an_agent_may_act_for_the_user_it_works_for(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();

    assert_eq!(
        f.scheduler()
            .list(&agent_for("agent", "alice"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        f.scheduler()
            .cancel(&JobId::new("job"), &agent_for("agent", "alice"))
            .await
            .unwrap()
    );
}

async fn replacing_a_job_as_a_delegate_does_not_take_it_over(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("job", after(60)), &user("alice"))
        .await
        .unwrap();

    f.scheduler()
        .schedule(spec("job", after(30)), &agent_for("agent", "alice"))
        .await
        .unwrap();

    assert_eq!(
        f.scheduler().list(&user("alice")).await.unwrap()[0].owner,
        PrincipalId::new("alice"),
        "an agent revising its user's job does not thereby acquire it"
    );
}

async fn listing_returns_only_what_the_caller_may_act_for(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    f.scheduler()
        .schedule(spec("alice-job", after(60)), &user("alice"))
        .await
        .unwrap();
    f.scheduler()
        .schedule(spec("mallory-job", after(60)), &user("mallory"))
        .await
        .unwrap();

    let listed: Vec<JobId> = f
        .scheduler()
        .list(&user("alice"))
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.spec.id)
        .collect();
    assert_eq!(
        listed,
        vec![JobId::new("alice-job")],
        "an enumeration that errored on someone else's job would report that it exists"
    );
}

async fn another_principal_cannot_cancel_a_firing_in_flight(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    f.scheduler()
        .schedule(spec("job", after(1)), &user("alice"))
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // The job has left the schedule by now -- it is a one-shot -- so this exercises the
    // authorisation the firing itself carries, which is the only record left of who owns it.
    let error = f
        .scheduler()
        .cancel(&JobId::new("job"), &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(!f.handler(HANDLER).observed_cancellation());

    f.handler(HANDLER).release();
}

async fn a_firing_does_not_inherit_its_owners_own_delegation(backend: Backend) {
    let f = fixture(backend, TestHandler::new()).await;
    // An agent working for Alice schedules a job. The job is the agent's.
    f.scheduler()
        .schedule(spec("job", after(1)), &agent_for("agent", "alice"))
        .await
        .unwrap();

    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    let principal = f.handler(HANDLER).calls()[0]
        .principal
        .clone()
        .expect("a firing is never anonymous");
    assert!(principal.may_act_for(&PrincipalId::new("agent")));
    assert!(
        !principal.may_act_for(&PrincipalId::new("alice")),
        "a stored job must not replay a delegation chain it never recorded"
    );
}

/// A job that leaves the schedule while still running frees its id, so the entry and the
/// firing under one id can belong to two different principals. Cancelling then has two
/// authorisations to pass, and failing the second one must not have already acted on the
/// first.
async fn a_refused_cancellation_leaves_the_schedule_alone(backend: Backend) {
    let f = fixture(backend, TestHandler::new().holding()).await;
    // Alice's one-shot fires and leaves the schedule; its firing is still in flight.
    f.scheduler()
        .schedule(spec("job", after(1)), &user("alice"))
        .await
        .unwrap();
    advance(Duration::from_secs(1)).await;
    f.handler(HANDLER).wait_for_calls(1).await;

    // The id is free again, so mallory may schedule under it.
    f.scheduler()
        .schedule(spec("job", after(600)), &user("mallory"))
        .await
        .unwrap();
    assert_eq!(f.scheduler().list(&user("mallory")).await.unwrap().len(), 1);

    // Cancelling is refused because alice's firing is still in flight...
    let error = f
        .scheduler()
        .cancel(&JobId::new("job"), &user("mallory"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);

    // ...but the refusal must not have removed mallory's job on the way out.
    assert_eq!(
        f.scheduler().list(&user("mallory")).await.unwrap().len(),
        1,
        "a refused cancellation cancels nothing"
    );

    f.handler(HANDLER).release();
}
