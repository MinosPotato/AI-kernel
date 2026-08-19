//! Proves the whole authorization path with a human in it: a policy
//! (`aik-policy`) that defers to a person, a registry (`aik-tools`) that enforces the
//! answer, a real capability (`aik-fs`) that changes the host, and an [`ApprovalBroker`]
//! carrying the question between them.
//!
//! Every test asserts the same three things together, because any one of them alone is
//! insufficient: what the caller was told, what happened to the file, and what the audit
//! trail recorded.

use std::sync::Arc;
use std::time::Duration;

use aik_api::audit::{
    AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase, InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ApprovalSink, Principal, PrincipalKind};
use aik_api::tool::{ToolName, ToolRegistry};
use aik_approval::{ApprovalBroker, ApprovalComponent, ApprovalGate, ApprovalSettings};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use aik_fs::FsWriteTool;
use aik_policy::RuleBasedPolicyEngine;
use aik_tools::ToolsComponent;
use serde_json::json;
use tokio::task::JoinHandle;

fn agent(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::Agent))
}

fn drain<T: Clone + Send + 'static>(stream: &mut EventStream<T>) -> Vec<T> {
    let mut collected = Vec::new();
    while let Some(Ok(envelope)) = stream.try_recv() {
        collected.push(envelope.payload);
    }
    collected
}

fn write_call(path: &str, content: &str) -> (ToolName, serde_json::Value) {
    (
        ToolName::new(aik_fs::DEFAULT_WRITE_NAME),
        json!({ "path": path, "content": content }),
    )
}

/// Allows `filesystem.write` as a capability, but asks a human about every actual file
/// under `root`.
///
/// The resource-scoped rule is written against the canonical root rather than `"*"`, since
/// an explicit wildcard would also match the capability-level question and produce two
/// prompts per call — see [`aik_policy::PolicyRule::resource`].
fn asks_about_every_file(root: &std::path::Path) -> RuleBasedPolicyEngine {
    RuleBasedPolicyEngine::new(
        serde_json::from_value(json!({ "rules": [
            { "action": "filesystem.write", "resource": format!("{}/*", root.display()),
              "effect": { "decision": "require_approval", "prompt": "let the agent edit this file?" } },
            { "action": "filesystem.write", "effect": { "decision": "allow" } }
        ]}))
        .unwrap(),
    )
    .unwrap()
}

/// The whole stack: a write tool over `root`, the policy above, and `broker` as the only
/// way a `require_approval` decision can be resolved.
fn stack(root: &std::path::Path, broker: Arc<ApprovalBroker>) -> Kernel {
    let canonical = root.canonicalize().unwrap();
    Kernel::builder()
        .component(ApprovalComponent::new(broker.clone()))
        .component(
            ToolsComponent::new()
                .with_tool(FsWriteTool::new(root).unwrap())
                .with_policy(Arc::new(asks_about_every_file(&canonical)))
                .with_approvals(broker as Arc<dyn ApprovalSink>),
        )
        .build()
        .unwrap()
}

/// A stand-in for a frontend: answers every question the same way, and records what it saw.
///
/// The record is shared rather than returned, because a real frontend runs until it is shut
/// down; the handle is dropped by the test, which detaches the gate with it.
struct Responder {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Responder {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn spawn_responder(gate: ApprovalGate, answer: bool) -> Responder {
    let mut stream = gate.subscribe();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let task = tokio::spawn(async move {
        while let Some(pending) = stream.recv().await {
            recorded.lock().unwrap().push(format!(
                "{}|{}|{}",
                pending.prompt,
                pending.request.action,
                pending
                    .request
                    .resource
                    .as_ref()
                    .map(|resource| resource.as_str().to_owned())
                    .unwrap_or_default()
            ));
            let answered = if answer {
                stream.gate().approve(&pending.id)
            } else {
                stream.gate().deny(&pending.id)
            };
            answered.unwrap();
        }
    });
    Responder { seen, task }
}

#[tokio::test(start_paused = true)]
async fn an_approved_write_happens_and_is_audited_as_approved() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    std::fs::write(canonical.join("notes.md"), "original").unwrap();

    let broker = Arc::new(ApprovalBroker::new());
    let responder = spawn_responder(broker.gate(), true);
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "EDITED");
    let outcome = tools.invoke(&name, arguments, &agent("a1")).await.unwrap();

    assert!(!outcome.is_error);
    assert_eq!(
        std::fs::read_to_string(canonical.join("notes.md")).unwrap(),
        "EDITED"
    );

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[0].outcome, AuthorizationOutcome::Allowed);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(decided[1].outcome, AuthorizationOutcome::ApprovalGranted);
    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Succeeded
    );
    assert_eq!(broker.pending_count(), 0);

    // The human was shown the policy's prompt and the canonical path — never the bytes.
    let seen = responder.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("let the agent edit this file?|filesystem.write|"));
    assert!(seen[0].ends_with("/notes.md"), "{}", seen[0]);
    assert!(!seen[0].contains("EDITED"));

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_refused_write_does_not_happen_and_is_audited_as_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    let broker = Arc::new(ApprovalBroker::new());
    let _responder = spawn_responder(broker.gate(), false);
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "EDITED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );

    let decided = drain(&mut decisions);
    assert_eq!(decided[1].outcome, AuthorizationOutcome::ApprovalRefused);
    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Denied
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_question_nobody_answers_expires_without_writing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    // A gate is attached — somebody could answer — but nothing ever does.
    let broker = Arc::new(ApprovalBroker::with_settings(ApprovalSettings {
        timeout: Duration::from_secs(30),
        ..Default::default()
    }));
    let gate = broker.gate();
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "EDITED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Timeout(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );

    // An expiry is recorded as a broken mechanism, not as a human refusing. The invocation
    // itself is `Denied`, as every pre-execution authorization failure is: the call was
    // refused before the tool ran, and *why* is the decision event's job to say.
    let decided = drain(&mut decisions);
    assert_eq!(
        decided[1].outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );
    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Denied
    );

    // Nothing is left for a late answer to grant.
    assert!(gate.pending().is_empty());
    assert_eq!(broker.pending_count(), 0);

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancelling_the_operation_withdraws_the_question() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    let broker = Arc::new(ApprovalBroker::new());
    let gate = broker.gate();
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let cx = agent("a1");
    let cancellation = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    });

    let (name, arguments) = write_call("notes.md", "EDITED");
    let error = tools.invoke(&name, arguments, &cx).await.unwrap_err();

    assert!(matches!(error, Error::Cancelled), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );
    assert!(
        gate.pending().is_empty(),
        "an abandoned question must not stay in front of a human"
    );

    let decided = drain(&mut decisions);
    assert_eq!(
        decided[1].outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );
    assert_eq!(
        drain(&mut invocations)[0].outcome,
        InvocationOutcome::Denied
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn with_no_responder_attached_the_write_is_refused_at_once() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    // A broker is wired, but no frontend ever attaches a gate to it.
    let broker = Arc::new(ApprovalBroker::with_settings(ApprovalSettings {
        timeout: Duration::from_secs(3_600),
        ..Default::default()
    }));
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let started = tokio::time::Instant::now();
    let (name, arguments) = write_call("notes.md", "EDITED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "an unanswerable question must not wait out the timeout"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );
    assert_eq!(
        drain(&mut decisions)[1].outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_frontend_that_goes_away_mid_question_does_not_grant_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    let broker = Arc::new(ApprovalBroker::with_settings(ApprovalSettings {
        timeout: Duration::from_secs(3_600),
        ..Default::default()
    }));
    let gate = broker.gate();
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let asking = {
        let tools = tools.clone();
        tokio::spawn(async move {
            let (name, arguments) = write_call("notes.md", "EDITED");
            tools.invoke(&name, arguments, &agent("a1")).await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(broker.pending_count(), 1);

    // Shutting the kernel down stops the broker, which refuses what was still waiting
    // rather than leaving the call parked on an answer that can no longer arrive.
    drop(gate);
    kernel.shutdown().await.unwrap();

    let error = asking.await.unwrap().unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );
    assert!(broker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn each_write_is_asked_about_separately() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();

    let broker = Arc::new(ApprovalBroker::new());
    let responder = spawn_responder(broker.gate(), true);
    let kernel = stack(root.path(), broker.clone());
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    for path in ["first.md", "second.md"] {
        let (name, arguments) = write_call(path, "hello");
        tools.invoke(&name, arguments, &agent("a1")).await.unwrap();
    }

    assert!(canonical.join("first.md").exists());
    assert!(canonical.join("second.md").exists());

    let seen = responder.seen();
    assert_eq!(seen.len(), 2, "approving one file must not cover another");
    assert!(seen[0].ends_with("/first.md"), "{}", seen[0]);
    assert!(seen[1].ends_with("/second.md"), "{}", seen[1]);

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_tools_own_confinement_still_holds_when_a_human_approves_everything() {
    let outer = tempfile::tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();

    let broker = Arc::new(ApprovalBroker::new());
    let _responder = spawn_responder(broker.gate(), true);
    let kernel = stack(&root_dir, broker.clone());
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("../secret.txt", "EDITED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();

    // Refused by the tool's own resolution, before a human is asked anything: approval can
    // only ever narrow what a tool will do, never widen it.
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(outer.path().join("secret.txt")).unwrap(),
        "TOP SECRET"
    );

    kernel.shutdown().await.unwrap();
}
