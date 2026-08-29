//! What a whole deployment does when the model provider has a bad moment.
//!
//! The mechanisms themselves — the backoff schedule, the breaker's state machine, what
//! establishing a stream retries and what it does not — are tested inside `aik-resilience`,
//! against a provider that fails on command. None of that says anything about the *seam*,
//! and the seam is where the interesting claims are:
//!
//! * that a deployment assembled by `aik-runtime` actually has the layer, without any
//!   frontend asking for it;
//! * that the agent loop and the compactor both reach the model *through* it, which depends
//!   entirely on component initialisation order and would fail silently if it were wrong;
//! * and that the spend ledger charges one turn per turn, however many upstream calls that
//!   turn took — the property that makes "retrying" and "a cumulative budget" safe to have at
//!   the same time.
//!
//! That last one cannot be asserted from inside either crate. `aik-quota` has never heard of
//! a retry and `aik-resilience` deliberately never touches a ledger; only a running agent
//! loop with both wired underneath it can show that the two compose.

mod support;

use std::sync::Arc;

use aik_api::agent::{Agent, AgentRequest, AgentUpdate};
use aik_api::execution::ExecutionContext;
use aik_api::model::{ModelId, ModelProvider};
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::quota::{QuotaDimension, QuotaGuard};
use aik_api::resilience::ProviderRetryScheduled;
use aik_core::clock::{ManualClock, Timestamp};
use aik_core::prelude::*;
use aik_core::{Config, ErrorKind};
use aik_runtime::{MemorySet, QUOTA_SECTION, RuntimeSettings, Storage, ToolSet};
use futures::StreamExt as _;
use serde_json::{Value, json};
use support::agent::{Reply, ScriptedModel};

/// 2026-08-28T14:35:12Z.
const FRIDAY: u64 = 1_787_927_712_000;

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

/// Retry settings with no waiting, so a suite asserts on *whether* a call is repeated.
///
/// The schedule's arithmetic is `aik-resilience`'s own business and is tested there; a
/// cross-subsystem test that also slept would be slow for no additional claim.
fn prompt_retries(max_attempts: u32) -> Value {
    json!({
        "components": {
            "model": {
                "resilient": {
                    "retry": {
                        "max_attempts": max_attempts,
                        "base_delay_ms": 0,
                        "max_delay_ms": 0,
                        "max_retry_after_ms": 0,
                    },
                    "breaker": { "failure_threshold": 0, "cooldown_ms": 0 },
                    "max_concurrent": 0,
                }
            }
        }
    })
}

/// A whole deployment, assembled by `aik-runtime` exactly as a frontend assembles one.
struct Deployment {
    kernel: Option<Kernel>,
    model: Arc<ScriptedModel>,
}

impl Deployment {
    async fn start(layers: Vec<Value>) -> Self {
        let root = std::env::temp_dir();
        let mut settings = RuntimeSettings::new(&root);
        settings.tools = ToolSet::None;
        settings.memory = MemorySet::Off;
        settings.model = Some(ModelId::new("test-model"));
        settings.model_component = ComponentId::new("model.stub");
        settings.storage = Storage::Ephemeral;

        let mut config = Config::builder();
        for layer in layers {
            config = config.layer(layer);
        }
        settings.config = config.build();

        let model = ScriptedModel::new();
        let (builder, _broker) = aik_runtime::builder(&settings, ModelId::new("test-model"))
            .expect("the deployment assembles");
        let kernel = builder
            .clock(Arc::new(ManualClock::new(Timestamp::from_millis(FRIDAY))))
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

    /// Runs the agent for one request against whatever is currently scripted.
    async fn ask(&self, cx: &ExecutionContext) -> Result<Vec<AgentUpdate>> {
        let agent = self.context().service::<dyn Agent>()?;
        let mut stream = agent.stream(AgentRequest::text("go"), cx).await?;
        let mut updates = Vec::new();
        while let Some(update) = stream.next().await {
            updates.push(update?);
        }
        Ok(updates)
    }

    /// How many completion requests actually reached the provider.
    fn upstream_calls(&self) -> usize {
        self.model.requests().len()
    }

    async fn stop(mut self) {
        if let Some(kernel) = self.kernel.take() {
            kernel.shutdown().await.expect("a clean shutdown");
        }
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        assert!(
            self.kernel.is_none() || std::thread::panicking(),
            "a deployment must be stopped rather than dropped"
        );
    }
}

fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
}

#[tokio::test]
async fn a_deployment_retries_without_any_frontend_asking_it_to() {
    let deployment = Deployment::start(vec![prompt_retries(3)]).await;
    deployment
        .model
        .script([Reply::Transient, Reply::Transient, Reply::answer("done")]);

    let updates = deployment.ask(&alice()).await.expect("the run completes");

    assert_eq!(
        deployment.upstream_calls(),
        3,
        "two failures and the answer that followed them"
    );
    let finished = updates
        .iter()
        .find_map(|update| match update {
            AgentUpdate::Finished(response) => Some(response),
            _ => None,
        })
        .expect("the run finished");
    assert_eq!(
        finished.output,
        vec![aik_api::model::ContentPart::text("done")],
        "the answer the third attempt returned is the one the caller got"
    );

    deployment.stop().await;
}

#[tokio::test]
async fn a_turn_is_charged_once_however_many_attempts_it_took() {
    // The property that makes retrying and a cumulative budget safe to have together. It
    // holds structurally rather than by arithmetic: retrying happens below the point where a
    // response exists to charge for, so there is never a second charge to reconcile.
    let deployment = Deployment::start(vec![
        prompt_retries(4),
        json!({ QUOTA_SECTION: {
            "limits": [{ "subject": "alice", "period": "day", "max_turns": 10 }]
        } }),
    ])
    .await;
    deployment.model.script([
        Reply::Transient,
        Reply::Transient,
        Reply::Transient,
        Reply::answer("done"),
    ]);

    deployment.ask(&alice()).await.expect("the run completes");
    assert_eq!(deployment.upstream_calls(), 4);

    let quota = deployment
        .context()
        .service::<dyn QuotaGuard>()
        .expect("a guard is always registered");
    let turns = quota
        .status(&alice())
        .await
        .expect("the ledger reads")
        .into_iter()
        .find(|status| status.dimension == QuotaDimension::Turns)
        .expect("a turns ceiling applies to alice");

    assert_eq!(
        turns.used, 1,
        "four upstream calls produced one model turn, and one turn is what a budget counts"
    );

    deployment.stop().await;
}

#[tokio::test]
async fn an_exhausted_budget_is_not_something_retrying_can_get_around() {
    // A refusal from the quota guard is not the service's fault, so nothing about it is
    // repeatable — and the check happens before the request is even assembled, so an
    // exhausted principal costs no upstream call at all.
    let deployment = Deployment::start(vec![
        prompt_retries(4),
        json!({ QUOTA_SECTION: {
            "limits": [{ "subject": "alice", "period": "day", "max_turns": 1 }]
        } }),
    ])
    .await;

    deployment.model.script([Reply::answer("first")]);
    deployment.ask(&alice()).await.expect("the first run fits");
    assert_eq!(deployment.upstream_calls(), 1);

    deployment.model.script([Reply::answer("second")]);
    let error = deployment
        .ask(&alice())
        .await
        .expect_err("the ceiling is reached");

    assert_eq!(error.kind(), ErrorKind::Permission, "{error}");
    assert_eq!(
        deployment.upstream_calls(),
        1,
        "a refused turn must cost no model call, retried or otherwise"
    );

    deployment.stop().await;
}

#[tokio::test]
async fn a_failure_the_provider_did_not_mark_ends_the_run_at_once() {
    let deployment = Deployment::start(vec![prompt_retries(5)]).await;
    deployment.model.script([Reply::Terminal]);

    let error = deployment.ask(&alice()).await.expect_err("the run fails");

    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    assert_eq!(
        deployment.upstream_calls(),
        1,
        "repeating a request the service refused on its merits spends the same tokens to be \
         told the same thing"
    );

    deployment.stop().await;
}

#[tokio::test]
async fn a_deployment_that_configured_a_pass_through_gets_one() {
    let deployment = Deployment::start(vec![prompt_retries(1)]).await;
    deployment.model.script([Reply::Transient]);

    deployment.ask(&alice()).await.expect_err("the run fails");

    assert_eq!(deployment.upstream_calls(), 1);

    deployment.stop().await;
}

#[tokio::test]
async fn retries_are_visible_to_whoever_is_watching_the_bus() {
    let deployment = Deployment::start(vec![prompt_retries(3)]).await;
    let mut retries = deployment.context().subscribe::<ProviderRetryScheduled>();

    deployment
        .model
        .script([Reply::Transient, Reply::answer("done")]);
    deployment.ask(&alice()).await.expect("the run completes");

    let event = retries
        .try_recv()
        .expect("a retry was published")
        .expect("no lag")
        .payload;
    assert_eq!(event.provider, ComponentId::new("model.stub"));
    assert_eq!(event.model, ModelId::new("test-model"));
    assert_eq!(event.attempt, 1);

    deployment.stop().await;
}

#[tokio::test]
async fn a_malformed_resilience_section_stops_the_kernel_rather_than_the_first_turn() {
    let root = std::env::temp_dir();
    let mut settings = RuntimeSettings::new(&root);
    settings.tools = ToolSet::None;
    settings.memory = MemorySet::Off;
    settings.model = Some(ModelId::new("test-model"));
    settings.model_component = ComponentId::new("model.stub");
    settings.storage = Storage::Ephemeral;
    settings.config = Config::builder()
        .layer(json!({
            "components": { "model": { "resilient": { "max_concurent": 2 } } }
        }))
        .build();

    let (builder, _broker) = aik_runtime::builder(&settings, ModelId::new("test-model"))
        .expect("the deployment assembles");
    let kernel = builder
        .component(StubModelComponent {
            model: ScriptedModel::new(),
        })
        .build()
        .expect("the kernel builds");

    let error = kernel.start().await.expect_err("a misspelled ceiling");
    let chain = format!("{:?}", aik_core::Error::wrap("start", error));
    assert!(
        chain.contains("max_concurent"),
        "the failure must name the key that is wrong: {chain}"
    );

    kernel.shutdown().await.expect("a clean shutdown");
}
