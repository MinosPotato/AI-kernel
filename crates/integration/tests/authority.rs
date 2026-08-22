//! What a scheduled job is allowed to actually *do*.
//!
//! The scheduler decides who owns a job; the policy engine decides what a principal may do;
//! the tool registry is where the two meet. Every one of those has its own suite, and the
//! seam between them had none — which mattered, because the seam is where a firing stops
//! being "Alice's job" and becomes a principal a policy has to recognise.
//!
//! A firing runs as [`RUN_PRINCIPAL`](aik_scheduler::RUN_PRINCIPAL) *acting for* the job's
//! owner. Two consequences this file pins down, in the order an operator meets them:
//!
//! 1. With no policy rule, an unattended firing can do nothing at all. Denial is the
//!    default, and it is the default for scheduled work specifically — not something the
//!    operator has to remember to configure.
//! 2. A rule naming only the scheduler grants *every* owner's jobs the same authority,
//!    because at this boundary they share one id. `on_behalf_of` is what tells them apart,
//!    and a rule that omits it is broader than it looks.

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind,
};
use aik_api::scheduler::{JobSpec, Scheduler, Trigger};
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::prelude::*;
use aik_core::{Config, ErrorKind, Result};
use aik_policy::RuleBasedPolicyEngine;
use serde_json::json;

mod support;
use support::{BoxFuture, HandlerComponent, RecordingHandler, until, user};

const HANDLER: &str = "jobs.tooluser";

/// The outcome one firing got when it tried to use a tool.
type Attempts = Arc<std::sync::Mutex<Vec<std::result::Result<(), ErrorKind>>>>;

/// Resolves the tool registry during `init`, the way a real subsystem would.
struct Resolver(Arc<std::sync::Mutex<Option<Arc<dyn ToolRegistry>>>>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish()
    }
}

#[async_trait]
impl Component for Resolver {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("jobs.tooluser.resolver").requires(aik_tools::DEFAULT_COMPONENT_ID)
    }
    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        *self.0.lock().unwrap() = Some(ctx.service::<dyn ToolRegistry>()?);
        Ok(())
    }
}

/// Runs a kernel in which a scheduled job reads a file through the real tool registry,
/// under `policy`, and reports what the tool boundary said.
async fn what_a_firing_may_do(
    policy: serde_json::Value,
    owner: &str,
) -> Vec<std::result::Result<(), ErrorKind>> {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("work");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("notes.md"), "hello").unwrap();
    let path = directory.path().join("aik.redb");

    let config = Config::builder()
        .layer(json!({
            "components": { "store": { "db": { "path": path } } },
            "policy": { "rules": policy },
        }))
        .build();

    let slot: Arc<std::sync::Mutex<Option<Arc<dyn ToolRegistry>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let attempts: Attempts = Arc::new(std::sync::Mutex::new(Vec::new()));

    let handler = {
        let slot = slot.clone();
        let attempts = attempts.clone();
        Arc::new(RecordingHandler::running(move |_spec, cx| -> BoxFuture {
            let slot = slot.clone();
            let attempts = attempts.clone();
            Box::pin(async move {
                let tools = slot.lock().unwrap().clone().expect("resolved in init");
                let outcome = tools
                    .invoke(
                        &ToolName::new(aik_fs::DEFAULT_NAME),
                        json!({ "path": "notes.md" }),
                        &cx,
                    )
                    .await;
                attempts
                    .lock()
                    .unwrap()
                    .push(outcome.map(|_| ()).map_err(|error| error.kind()));
                Ok::<(), aik_core::Error>(())
            })
        }))
    };

    let engine = RuleBasedPolicyEngine::from_config(&config, "policy").expect("a valid policy");
    let kernel = Kernel::builder()
        .config(config)
        .component(aik_store::StoreComponent::new())
        .component(
            aik_tools::ToolsComponent::new()
                .with_policy(Arc::new(engine))
                .with_tool(aik_fs::FsReadTool::new(&root).unwrap()),
        )
        .component(Resolver(slot.clone()))
        .component(
            HandlerComponent::new(HANDLER, handler).requiring(aik_tools::DEFAULT_COMPONENT_ID),
        )
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new(
                "job",
                Trigger::After {
                    delay: Duration::from_millis(20),
                },
                HANDLER,
            ),
            &user(owner),
        )
        .await
        .unwrap();

    until("the job to try the tool", async || {
        !attempts.lock().unwrap().is_empty()
    })
    .await;
    let observed = attempts.lock().unwrap().clone();
    kernel.shutdown().await.unwrap();
    observed
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_firing_may_do_nothing_by_default() {
    let observed = what_a_firing_may_do(json!([]), "alice").await;
    assert_eq!(
        observed.len(),
        1,
        "the job ran and reached the tool boundary"
    );
    assert_eq!(
        observed[0],
        Err(ErrorKind::Permission),
        "an empty policy denies scheduled work, rather than letting unattended code inherit \
         whatever the process can reach"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rule_written_for_a_person_does_not_reach_the_jobs_they_scheduled() {
    // The mistake an operator makes first: "alice may read files."
    let rules = json!([
        { "principal": { "id": "alice" }, "action": "filesystem.read", "effect": { "decision": "allow" } },
        { "principal": { "id": "alice" }, "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
    ]);
    assert_eq!(
        what_a_firing_may_do(rules, "alice").await[0],
        Err(ErrorKind::Permission),
        "a firing is not its owner, so authority granted to the owner does not silently \
         become authority for everything scheduled in their name"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn naming_the_delegator_is_what_grants_a_firing_its_owners_work() {
    let rules = |owner: &str| {
        json!([
            { "principal": { "id": aik_scheduler::RUN_PRINCIPAL, "on_behalf_of": owner },
              "action": "filesystem.read", "effect": { "decision": "allow" } },
            { "principal": { "id": aik_scheduler::RUN_PRINCIPAL, "on_behalf_of": owner },
              "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
        ])
    };

    assert_eq!(
        what_a_firing_may_do(rules("alice"), "alice").await[0],
        Ok(()),
        "the rule names the job's owner, so the job may run"
    );
    assert_eq!(
        what_a_firing_may_do(rules("alice"), "bob").await[0],
        Err(ErrorKind::Permission),
        "the same rule, a different owner: this is the distinction the policy layer could \
         not previously draw, and every scheduled job shared one authority without it"
    );
}

/// The unit-level truth table behind the integration assertions above.
///
/// Kept here rather than in `aik-policy` because what makes these cases *matter* is which
/// subsystems mint these principals, and that is knowledge this crate has and that one
/// deliberately does not.
#[tokio::test]
async fn the_matcher_tells_delegates_apart_without_breaking_rules_that_predate_it() {
    let engine = |rules: serde_json::Value| {
        let config = Config::builder()
            .layer(json!({ "policy": { "rules": rules } }))
            .build();
        RuleBasedPolicyEngine::from_config(&config, "policy").expect("a valid policy")
    };
    let ask = |principal: Principal| PermissionRequest {
        principal,
        action: ActionId::new("filesystem.read"),
        resource: None,
        context: serde_json::Value::Null,
    };
    let allow = |matcher: serde_json::Value| json!([{ "principal": matcher, "action": "filesystem.read", "effect": { "decision": "allow" } }]);

    let autonomous = Principal::new("scheduler", PrincipalKind::System);
    let for_alice = Principal::new("scheduler", PrincipalKind::System).on_behalf_of("alice");
    let for_bob = Principal::new("scheduler", PrincipalKind::System).on_behalf_of("bob");
    let cx = ExecutionContext::new();

    // Omitted: matches every delegation state, which is what every rule written before this
    // field existed has to keep meaning.
    let e = engine(allow(json!({ "id": "scheduler" })));
    for principal in [&autonomous, &for_alice, &for_bob] {
        assert_eq!(
            e.evaluate(&ask(principal.clone()), &cx).await.unwrap(),
            Decision::Allow,
            "an omitted on_behalf_of constrains nothing: {principal:?}"
        );
    }

    // Named: exactly that delegator, and nobody else -- not even the undelegated principal.
    let e = engine(allow(json!({ "id": "scheduler", "on_behalf_of": "alice" })));
    assert_eq!(
        e.evaluate(&ask(for_alice.clone()), &cx).await.unwrap(),
        Decision::Allow
    );
    assert!(matches!(
        e.evaluate(&ask(for_bob.clone()), &cx).await.unwrap(),
        Decision::Deny { .. }
    ));
    assert!(matches!(
        e.evaluate(&ask(autonomous.clone()), &cx).await.unwrap(),
        Decision::Deny { .. }
    ));

    // `"*"` reads "some delegate", so it separates delegated work from autonomous work.
    let e = engine(allow(json!({ "id": "scheduler", "on_behalf_of": "*" })));
    assert_eq!(
        e.evaluate(&ask(for_alice), &cx).await.unwrap(),
        Decision::Allow
    );
    assert_eq!(
        e.evaluate(&ask(for_bob), &cx).await.unwrap(),
        Decision::Allow
    );
    assert!(
        matches!(
            e.evaluate(&ask(autonomous), &cx).await.unwrap(),
            Decision::Deny { .. }
        ),
        "a principal acting for nobody matches no present on_behalf_of, `*` included"
    );

    // Prefixes work here as they do on every other axis.
    let e = engine(allow(
        json!({ "id": "scheduler", "on_behalf_of": "team-*" }),
    ));
    let member = Principal::new("scheduler", PrincipalKind::System).on_behalf_of("team-ops");
    let outsider = Principal::new("scheduler", PrincipalKind::System).on_behalf_of("teamster");
    assert_eq!(
        e.evaluate(&ask(member), &cx).await.unwrap(),
        Decision::Allow
    );
    assert!(matches!(
        e.evaluate(&ask(outsider), &cx).await.unwrap(),
        Decision::Deny { .. }
    ));

    // And the agent, the other delegating subsystem, is told apart the same way.
    let e = engine(allow(json!({ "id": "assistant", "on_behalf_of": "alice" })));
    let hers = Principal::new("assistant", PrincipalKind::Agent).on_behalf_of("alice");
    let his = Principal::new("assistant", PrincipalKind::Agent).on_behalf_of("bob");
    assert_eq!(e.evaluate(&ask(hers), &cx).await.unwrap(), Decision::Allow);
    assert!(matches!(
        e.evaluate(&ask(his), &cx).await.unwrap(),
        Decision::Deny { .. }
    ));
}
