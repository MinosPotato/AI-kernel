//! What a principal may spend, across runs, across sessions, across restarts.
//!
//! Every other bound in this system is per run. [`AgentLoopSettings`](aik_agent::AgentLoopSettings)
//! stops one conversation at sixteen model turns and sixty-four tool calls, and then the next
//! run starts at zero again. That is the right shape for what it is — it stops a loop, not a
//! budget — and it is exactly the wrong shape for the question an operator actually has, which
//! is how much this deployment may spend in a day.
//!
//! So the suite here is about the seam between three subsystems that each know part of the
//! answer and none of which could enforce it alone: the agent loop knows a turn is about to be
//! taken and what it cost, `aik-quota` knows what the ceilings are, and `aik-store` is what
//! makes a ceiling mean something after a restart. The interesting assertions are all about
//! the *joins*: that a refusal costs no model call, that a delegated turn lands on two
//! ledgers, that the wiring reads one configuration section, and that stopping the process is
//! not a way to reset a budget.

mod support;

use std::path::Path;
use std::sync::Arc;

use aik_api::agent::{Agent, AgentRequest, AgentUpdate};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ModelId, ModelProvider};
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::quota::{QuotaDimension, QuotaGuard};
use aik_core::clock::{ManualClock, Timestamp};
use aik_core::prelude::*;
use aik_core::{Config, ErrorKind};
use aik_quota::{QuotaDocument, QuotaPeriod};
use aik_runtime::{MemorySet, QUOTA_SECTION, RuntimeSettings, Storage, ToolSet};
use futures::StreamExt as _;
use serde_json::{Value, json};
use support::agent::{Reply, ScriptedModel};

/// 2026-08-28T14:35:12Z.
const FRIDAY: u64 = 1_787_927_712_000;
const DAY: u64 = 24 * 60 * 60 * 1_000;

/// Publishes the scripted model as this kernel's `dyn ModelProvider`.
#[derive(Debug)]
struct StubModelComponent {
    model: Arc<ScriptedModel>,
}

#[async_trait]
impl Component for StubModelComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("model.stub").described("a scripted model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn ModelProvider>(self.model.clone())
    }
}

/// A whole deployment, assembled by `aik-runtime` exactly as a frontend assembles one.
struct Deployment {
    kernel: Option<Kernel>,
    model: Arc<ScriptedModel>,
}

impl Deployment {
    /// Starts a deployment whose ceilings are `quota`, keeping state at `database` if given.
    async fn start(quota: Value, database: Option<&Path>, clock: Arc<ManualClock>) -> Self {
        // The clock is the kernel's, not this fixture's: a window boundary is something the
        // guard derives from it, so a test that moves time moves the handle it kept.
        let root = std::env::temp_dir();
        let mut settings = RuntimeSettings::new(&root);
        settings.tools = ToolSet::None;
        settings.memory = MemorySet::Off;
        settings.model = Some(ModelId::new("test-model"));
        settings.model_component = ComponentId::new("model.stub");
        settings.storage = match database {
            Some(path) => Storage::Persistent(path.to_path_buf()),
            None => Storage::Ephemeral,
        };
        settings.config = Config::builder()
            .layer(json!({
                "components": { "store": { "db": { "path": database } } },
                QUOTA_SECTION: quota,
            }))
            .build();

        let model = ScriptedModel::new();
        let (builder, _broker) = aik_runtime::builder(&settings, ModelId::new("test-model"))
            .expect("the deployment assembles");
        let kernel = builder
            .clock(clock.clone())
            .component(StubModelComponent {
                model: model.clone(),
            })
            .build()
            .expect("the kernel builds");
        kernel.start().await.expect("the kernel starts");

        Self {
            kernel: Some(kernel),
            model,
        }
    }

    fn context(&self) -> KernelContext {
        self.kernel.as_ref().expect("running").context()
    }

    /// Runs the agent for one request, with one scripted final answer waiting.
    async fn ask(&self, cx: &ExecutionContext) -> Result<Vec<AgentUpdate>> {
        self.model.script([Reply::answer("done")]);
        let agent = self.context().service::<dyn Agent>()?;
        let mut stream = agent.stream(AgentRequest::text("go"), cx).await?;
        let mut updates = Vec::new();
        while let Some(update) = stream.next().await {
            updates.push(update?);
        }
        Ok(updates)
    }

    async fn stop(mut self) {
        if let Some(kernel) = self.kernel.take() {
            kernel.shutdown().await.expect("a clean shutdown");
        }
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        // A kernel dropped without shutting down would leave the database locked for the next
        // one, which is exactly what the restart tests here are about. Silent while the
        // thread is already unwinding, so a failed assertion is what the runner reports
        // rather than this.
        assert!(
            self.kernel.is_none() || std::thread::panicking(),
            "a deployment must be stopped rather than dropped"
        );
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(Timestamp::from_millis(FRIDAY)))
}

/// The agent acting for a person, which is what every real run looks like.
fn assistant_for(user: &str) -> ExecutionContext {
    ExecutionContext::new()
        .with_principal(Principal::new("assistant", PrincipalKind::Agent).on_behalf_of(user))
}

fn two_turns_a_day() -> Value {
    json!({ "limits": [{ "subject": "*", "period": "day", "max_turns": 2 }] })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ceiling_configured_in_one_section_reaches_the_agent_the_runtime_assembled() {
    let deployment = Deployment::start(two_turns_a_day(), None, clock()).await;
    let cx = assistant_for("alice");

    for _ in 0..2 {
        deployment.ask(&cx).await.expect("within budget");
    }
    let error = deployment.ask(&cx).await.unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(error.to_string().contains("2 of 2 model turns"), "{error}");
    assert_eq!(
        deployment.model.requests().len(),
        2,
        "a refused turn must not reach the provider"
    );
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_counter_starts_again_when_the_window_does() {
    let clock = clock();
    let deployment = Deployment::start(two_turns_a_day(), None, clock.clone()).await;
    let cx = assistant_for("alice");

    for _ in 0..2 {
        deployment.ask(&cx).await.expect("within budget");
    }
    assert!(deployment.ask(&cx).await.is_err());

    clock.set(Timestamp::from_millis(FRIDAY + DAY));
    deployment.ask(&cx).await.expect("a new day, a new budget");
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_budget_is_not_reset_by_restarting_the_thing_it_constrains() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let cx = assistant_for("alice");

    let deployment = Deployment::start(two_turns_a_day(), Some(&path), clock()).await;
    for _ in 0..2 {
        deployment.ask(&cx).await.expect("within budget");
    }
    deployment.stop().await;

    // A second process, the same day, over the same database.
    let deployment = Deployment::start(two_turns_a_day(), Some(&path), clock()).await;
    let error = deployment.ask(&cx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert_eq!(
        deployment.model.requests().len(),
        0,
        "restarting is not a way to buy two more turns"
    );
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_deployment_keeps_its_ledger_only_while_it_runs() {
    let cx = assistant_for("alice");

    let deployment = Deployment::start(two_turns_a_day(), None, clock()).await;
    for _ in 0..2 {
        deployment.ask(&cx).await.expect("within budget");
    }
    assert!(deployment.ask(&cx).await.is_err());
    deployment.stop().await;

    // The documented consequence of writing nothing to disk, asserted rather than assumed:
    // an `--ephemeral` run is bounded while it runs and starts fresh when it is started
    // fresh. A deployment that needs otherwise chooses durable storage.
    let deployment = Deployment::start(two_turns_a_day(), None, clock()).await;
    deployment
        .ask(&cx)
        .await
        .expect("a new process, a new ledger");
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_turn_is_counted_against_the_agent_and_against_the_person() {
    let quota = json!({
        "limits": [
            { "subject": "assistant", "period": "day", "max_turns": 10 },
            { "subject": "alice", "period": "day", "max_turns": 1 },
            { "subject": "bob", "period": "day", "max_turns": 1 }
        ]
    });
    let deployment = Deployment::start(quota, None, clock()).await;

    deployment
        .ask(&assistant_for("alice"))
        .await
        .expect("alice's first");
    let error = deployment.ask(&assistant_for("alice")).await.unwrap_err();
    assert!(
        error.to_string().contains("alice"),
        "the person's ceiling is what stopped it, not the agent's: {error}"
    );

    // The same agent, acting for somebody else, still has budget: the ceilings are per
    // identity, and the agent's own is nowhere near spent.
    deployment
        .ask(&assistant_for("bob"))
        .await
        .expect("bob's first");
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_tells_an_operator_what_is_left_without_spending_any_of_it() {
    let deployment = Deployment::start(two_turns_a_day(), None, clock()).await;
    let cx = assistant_for("alice");
    deployment.ask(&cx).await.expect("within budget");

    let guard = deployment.context().service::<dyn QuotaGuard>().unwrap();
    let status = guard.status(&cx).await.unwrap();

    // One rule, matching both identities in play, so two counters — and both saw the turn.
    assert_eq!(status.len(), 2);
    for entry in &status {
        assert_eq!(entry.dimension, QuotaDimension::Turns);
        assert_eq!(entry.used, 1);
        assert_eq!(entry.limit, 2);
        assert_eq!(entry.remaining(), 1);
        assert!(entry.window.starts_with("day:"));
    }
    let subjects: Vec<&str> = status.iter().map(|entry| entry.subject.as_str()).collect();
    assert!(subjects.contains(&"assistant") && subjects.contains(&"alice"));

    // Reporting is not enforcement: the turn that was left is still there.
    deployment
        .ask(&cx)
        .await
        .expect("reading a budget does not spend it");
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deployment_that_configures_nothing_is_bounded_by_its_run_settings_alone() {
    let deployment = Deployment::start(json!({}), None, clock()).await;
    let cx = assistant_for("alice");
    for _ in 0..5 {
        deployment.ask(&cx).await.expect("nothing is capped");
    }

    let guard = deployment.context().service::<dyn QuotaGuard>().unwrap();
    assert!(
        guard.status(&cx).await.unwrap().is_empty(),
        "an unconfigured quota reports no ceilings rather than inventing defaults"
    );
    deployment.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_ceiling_stops_the_deployment_rather_than_the_first_turn() {
    let root = std::env::temp_dir();
    let mut settings = RuntimeSettings::new(&root);
    settings.model_component = ComponentId::new("model.stub");
    settings.config = Config::builder()
        .layer(json!({
            QUOTA_SECTION: { "limits": [{ "subject": "*", "period": "day", "max_turns": 0 }] }
        }))
        .build();

    let error = aik_runtime::builder(&settings, ModelId::new("test-model")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.to_string().contains("quota.limits[0]"), "{error}");
}

#[test]
fn the_shipped_example_configuration_carries_ceilings_that_actually_bind() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cli")
        .join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("the example configuration is readable");
    let file: Value = serde_json::from_str(&text).expect("the example configuration is valid JSON");
    let document: QuotaDocument =
        serde_json::from_value(file[QUOTA_SECTION].clone()).expect("a quota document");
    document.validate().expect("the shipped ceilings are valid");

    // The autonomous identity is capped more tightly than anybody attended, which is the
    // whole reason the example names it separately.
    let scheduler = document
        .limits
        .iter()
        .find(|rule| rule.subject.matches("scheduler") && rule.period == QuotaPeriod::Hour)
        .expect("the schedule is capped by the hour");
    assert!(scheduler.max_turns.is_some());

    // Every price key is one that matches something. A price written as `claude-` rather than
    // `claude-*` is an exact match that no model id has ever equalled, which would leave a
    // cost ceiling refusing every Anthropic turn instead of pricing it.
    let priced: Vec<&String> = document.prices.keys().collect();
    assert!(
        !priced.is_empty(),
        "a cost ceiling needs prices to measure with"
    );
    for model in ["claude-opus-5", "claude-sonnet-4-5", "llama3.1:8b"] {
        assert!(
            document.price(model).is_some(),
            "`{model}` has no price, so a cost ceiling would refuse it outright"
        );
    }
    assert!(
        document
            .price("claude-opus-5")
            .unwrap()
            .input_micros_per_million
            > 0,
        "a hosted model priced at zero would make the monthly ceiling unreachable"
    );
}
