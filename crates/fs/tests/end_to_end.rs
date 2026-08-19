//! Proves the vertical slice: `ToolRegistry` (`aik-tools`) + a real `PolicyEngine`
//! (`aik-policy`) + `FsReadTool` and `FsWriteTool` (this crate), wired together exactly as a
//! deployment would, with authorization, confinement and audit all active at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aik_api::audit::{
    AuthorizationDecided, AuthorizationOutcome, AuthorizationPhase, InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ApprovalSink, PermissionRequest, Principal, PrincipalKind, ResourceId};
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use aik_fs::{FsListTool, FsReadTool, FsWriteTool};
use aik_policy::RuleBasedPolicyEngine;
use aik_tools::ToolsComponent;
use async_trait::async_trait;
use serde_json::json;

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

fn policy_allowing_root_but_not_secrets(root: &str) -> RuleBasedPolicyEngine {
    let document = serde_json::from_value(json!({ "rules": [
        { "action": "filesystem.read", "resource": format!("{root}/secrets/*"),
          "effect": { "decision": "deny", "reason": "secret material" } },
        { "action": "filesystem.read", "resource": format!("{root}/*"),
          "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ]}))
    .unwrap();
    RuleBasedPolicyEngine::new(document).unwrap()
}

#[tokio::test]
async fn a_permitted_read_flows_through_the_whole_stack() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello, world").unwrap();
    std::fs::create_dir(root.path().join("secrets")).unwrap();
    std::fs::write(root.path().join("secrets/token"), "sk-secret").unwrap();

    let tool = FsReadTool::new(root.path()).unwrap();
    let root_str = tool.root().to_string_lossy().into_owned();
    let policy = policy_allowing_root_but_not_secrets(&root_str);

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(tool)
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let outcome = tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap();
    assert_eq!(outcome.output["content"], json!("hello, world"));

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_narrows_the_tools_own_root_and_the_denial_is_audited() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("secrets")).unwrap();
    std::fs::write(root.path().join("secrets/token"), "sk-secret").unwrap();

    let tool = FsReadTool::new(root.path()).unwrap();
    let root_str = tool.root().to_string_lossy().into_owned();
    let policy = policy_allowing_root_but_not_secrets(&root_str);

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(tool)
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "secrets/token" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");

    let decided = drain(&mut decisions);
    // Capability-level, then resource-level (declared via `planned_resources`) — the
    // resource-level rule is the one that refuses it. No `discovered_resource` phase is
    // needed since `FsReadTool` refuses anything it would only find out about mid-run.
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert!(
        decided[1]
            .resource
            .as_ref()
            .unwrap()
            .as_str()
            .ends_with("/secrets/token")
    );

    // The secret contents never appear anywhere in the audit trail: only the path.
    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_tools_own_confinement_holds_even_when_policy_would_allow_everything() {
    // A policy that allows the action outright must still not let the tool escape its own
    // configured root — enforcement is independent of, and cannot be loosened by, policy.
    let outer = tempfile::tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();

    let tool = FsReadTool::new(&root_dir).unwrap();
    let document = serde_json::from_value(json!({ "rules": [
        { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ]}))
    .unwrap();
    let policy = RuleBasedPolicyEngine::new(document).unwrap();

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(tool)
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "../secret.txt" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();
    // `planned_resources` refuses the traversal before any policy question is even asked,
    // so this surfaces as the tool's own structural refusal, not a policy denial.
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_policy_configured_denies_reads_even_though_the_root_would_permit_them() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(tool))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Writes: the same stack, with a capability that changes the host
// ---------------------------------------------------------------------------

/// An approval sink that answers every prompt the same way, and counts how often it was
/// asked — so a test can tell "denied without asking" apart from "asked and refused".
struct Answers {
    answer: bool,
    asked: AtomicUsize,
}

impl Answers {
    fn new(answer: bool) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ApprovalSink for Answers {
    async fn request_approval(
        &self,
        _request: &PermissionRequest,
        _prompt: &str,
        _cx: &ExecutionContext,
    ) -> Result<bool> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer)
    }
}

/// Builds a kernel hosting a read tool and a write tool over the same root, under `policy`.
fn stack(root: &std::path::Path, policy: RuleBasedPolicyEngine) -> Kernel {
    Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsReadTool::new(root).unwrap())
                .with_tool(FsWriteTool::new(root).unwrap())
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap()
}

fn engine(rules: serde_json::Value) -> RuleBasedPolicyEngine {
    RuleBasedPolicyEngine::new(serde_json::from_value(rules).unwrap()).unwrap()
}

fn write_call(path: &str, content: &str) -> (ToolName, serde_json::Value) {
    (
        ToolName::new(aik_fs::DEFAULT_WRITE_NAME),
        json!({ "path": path, "content": content }),
    )
}

#[tokio::test]
async fn a_permitted_write_flows_through_the_whole_stack_and_is_audited() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let kernel = stack(
        root.path(),
        engine(json!({ "rules": [
            { "action": "filesystem.write", "resource": format!("{}/*", canonical.display()),
              "effect": { "decision": "allow" } },
            { "action": "filesystem.write", "effect": { "decision": "allow" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "hello, world");
    let outcome = tools.invoke(&name, arguments, &agent("a1")).await.unwrap();
    assert!(!outcome.is_error);
    assert_eq!(
        std::fs::read_to_string(canonical.join("notes.md")).unwrap(),
        "hello, world"
    );

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert!(decided.iter().all(|d| d.outcome.is_allowed()));
    // The written bytes never reach the audit trail; only the path does.
    let resource = decided[1].resource.as_ref().unwrap().as_str().to_owned();
    assert!(resource.ends_with("/notes.md"));

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::Succeeded);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_to_read_does_not_imply_permission_to_write() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();
    let kernel = stack(
        root.path(),
        engine(json!({ "rules": [
            { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
            { "action": "filesystem.read", "effect": { "decision": "allow" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    // The read the same policy permits still works…
    tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap();
    drain(&mut decisions);

    // …and the write does not.
    let (name, arguments) = write_call("notes.md", "OVERWRITTEN");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );

    // Refused at capability level, so the resource was never even reached.
    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert!(!decided[0].outcome.is_allowed());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_can_carve_a_read_only_region_out_of_a_writable_root() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    std::fs::create_dir(canonical.join("vendor")).unwrap();
    std::fs::write(canonical.join("vendor/pinned.txt"), "do not touch").unwrap();
    let kernel = stack(
        root.path(),
        engine(json!({ "rules": [
            { "action": "filesystem.write", "resource": format!("{}/vendor/*", canonical.display()),
              "effect": { "decision": "deny", "reason": "vendored code is not editable" } },
            { "action": "filesystem.write", "resource": format!("{}/*", canonical.display()),
              "effect": { "decision": "allow" } },
            { "action": "filesystem.write", "effect": { "decision": "allow" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("vendor/pinned.txt", "EDITED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(canonical.join("vendor/pinned.txt")).unwrap(),
        "do not touch"
    );

    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(
        decided[1].outcome,
        AuthorizationOutcome::Denied {
            reason: "vendored code is not editable".into()
        }
    );

    // A file outside that region is still writable, so the denial was about the resource
    // and not about the capability.
    let (name, arguments) = write_call("notes.md", "fine");
    tools.invoke(&name, arguments, &agent("a1")).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(canonical.join("notes.md")).unwrap(),
        "fine"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_policy_configured_denies_writes_and_leaves_the_file_alone() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();

    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(FsWriteTool::new(root.path()).unwrap()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "OVERWRITTEN");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );
    assert_eq!(
        drain(&mut decisions)[0].outcome,
        AuthorizationOutcome::PolicyUnavailable
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_write_needing_approval_is_denied_when_no_approval_sink_is_configured() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();
    let kernel = stack(
        root.path(),
        engine(json!({ "rules": [
            { "action": "filesystem.write", "resource": "*",
              "effect": { "decision": "require_approval", "prompt": "let the agent edit this?" } },
            { "action": "filesystem.write",
              "effect": { "decision": "require_approval", "prompt": "let the agent write?" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("notes.md", "OVERWRITTEN");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
        "original"
    );
    assert_eq!(
        drain(&mut decisions)[0].outcome,
        AuthorizationOutcome::ApprovalUnavailable
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_refused_approval_leaves_the_file_untouched_and_a_granted_one_does_not() {
    for (answer, expected) in [(false, "original"), (true, "OVERWRITTEN")] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.md"), "original").unwrap();
        let approvals = Answers::new(answer);
        let policy = engine(json!({ "rules": [
            { "action": "filesystem.write", "resource": "*",
              "effect": { "decision": "require_approval", "prompt": "let the agent edit this?" } },
            { "action": "filesystem.write",
              "effect": { "decision": "require_approval", "prompt": "let the agent write?" } }
        ]}));

        let kernel = Kernel::builder()
            .component(
                ToolsComponent::new()
                    .with_tool(FsWriteTool::new(root.path()).unwrap())
                    .with_policy(Arc::new(policy))
                    .with_approvals(approvals.clone()),
            )
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

        let (name, arguments) = write_call("notes.md", "OVERWRITTEN");
        let result = tools.invoke(&name, arguments, &agent("a1")).await;
        assert_eq!(result.is_ok(), answer, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("notes.md")).unwrap(),
            expected
        );
        // A human was actually consulted rather than the decision being assumed.
        assert!(approvals.asked.load(Ordering::SeqCst) >= 1);

        kernel.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn the_write_tools_confinement_holds_even_when_policy_allows_everything() {
    let outer = tempfile::tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();

    let kernel = stack(
        &root_dir,
        engine(json!({ "rules": [
            { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
            { "action": "*", "effect": { "decision": "allow" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("../secret.txt", "OVERWRITTEN");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    // Refused by the tool's own resolution, before any policy question is asked.
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(outer.path().join("secret.txt")).unwrap(),
        "TOP SECRET"
    );

    kernel.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_parent_escape_is_audited_as_failed_not_denied_even_with_a_permissive_policy() {
    // Combines two independent guarantees this crate and `aik-tools` each make: the write
    // tool refuses to resolve a claim through a symlinked parent that escapes its root,
    // regardless of policy (this test's `*`/`*` rules would authorize anything a resource
    // claim named), and `InProcessToolRegistry` records that refusal as
    // `Failed { kind: "confinement" }` rather than `Denied`, because `planned_resources`
    // fails before any policy question is asked about it — see
    // `InProcessToolRegistry::invoke` in `crates/tools/src/registry.rs`.
    let outer = tempfile::tempdir().unwrap();
    let root_dir = outer.path().join("root");
    let elsewhere = outer.path().join("elsewhere");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root_dir.join("escape")).unwrap();

    let kernel = stack(
        &root_dir,
        engine(json!({ "rules": [
            { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
            { "action": "*", "effect": { "decision": "allow" } }
        ]})),
    );
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = write_call("escape/planted.txt", "PLANTED");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Confinement(_)), "{error}");
    assert!(!elsewhere.join("planted.txt").exists());

    // No policy question was ever asked — the tool never produced a resource claim to ask
    // about, so there is nothing for a permissive policy to have authorized in the first
    // place.
    assert!(drain(&mut decisions).is_empty());

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(
        invoked[0].outcome,
        InvocationOutcome::Failed {
            kind: "confinement".into()
        }
    );

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Listing: the first tool that authorizes resources it only discovers by running
// ---------------------------------------------------------------------------

fn list_call(path: &str) -> (ToolName, serde_json::Value) {
    (
        ToolName::new(aik_fs::DEFAULT_LIST_NAME),
        json!({ "path": path }),
    )
}

#[tokio::test]
async fn a_permitted_listing_flows_through_the_whole_stack_and_is_audited() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let canonical = root.path().canonicalize().unwrap();

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsListTool::new(root.path()).unwrap())
                .with_policy(Arc::new(engine(json!({ "rules": [
                    { "action": "filesystem.list", "resource": format!("{}/*", canonical.display()),
                      "effect": { "decision": "allow" } },
                    { "action": "filesystem.list", "resource": canonical.display().to_string(),
                      "effect": { "decision": "allow" } },
                    { "action": "filesystem.list", "effect": { "decision": "allow" } }
                ]})))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = list_call("");
    let outcome = tools.invoke(&name, arguments, &agent("a1")).await.unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.output["entries"][0]["name"], json!("notes.md"));

    let decided = drain(&mut decisions);
    // Capability (Tool), the directory itself (Resource), then one DiscoveredResource
    // decision per entry found while actually reading the directory.
    assert_eq!(decided.len(), 3);
    assert_eq!(decided[0].phase, AuthorizationPhase::Tool);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert_eq!(
        decided[1].resource,
        Some(ResourceId::new(canonical.to_string_lossy()))
    );
    assert_eq!(decided[2].phase, AuthorizationPhase::DiscoveredResource);
    assert!(
        decided[2]
            .resource
            .as_ref()
            .unwrap()
            .as_str()
            .ends_with("/notes.md")
    );
    assert!(decided.iter().all(|d| d.outcome.is_allowed()));

    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::Succeeded);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_narrows_which_discovered_entries_are_visible_without_failing_the_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    std::fs::create_dir(root.path().join("secrets")).unwrap();
    std::fs::write(root.path().join("secrets/token"), "sk-secret").unwrap();
    let canonical = root.path().canonicalize().unwrap();

    let policy = engine(json!({ "rules": [
        { "action": "filesystem.list", "resource": format!("{}/secrets", canonical.display()),
          "effect": { "decision": "deny", "reason": "secret directory" } },
        { "action": "filesystem.list", "resource": format!("{}/*", canonical.display()),
          "effect": { "decision": "allow" } },
        { "action": "filesystem.list", "resource": canonical.display().to_string(),
          "effect": { "decision": "allow" } },
        { "action": "filesystem.list", "effect": { "decision": "allow" } }
    ]}));

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsListTool::new(root.path()).unwrap())
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let mut invocations = kernel.context().subscribe::<ToolInvoked>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = list_call("");
    let outcome = tools.invoke(&name, arguments, &agent("a1")).await.unwrap();

    // The call succeeded — narrowing a discovered resource is not a call failure — and only
    // the entry policy allows is present in the result.
    assert!(!outcome.is_error);
    let names: std::collections::BTreeSet<String> = outcome.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        std::collections::BTreeSet::from(["notes.md".to_owned()])
    );

    // But the denial was not silent: it is in the audit trail, under its own phase, with the
    // policy's reason.
    let decided = drain(&mut decisions);
    let denied = decided
        .iter()
        .find(|d| d.phase == AuthorizationPhase::DiscoveredResource && !d.outcome.is_allowed())
        .expect("the secrets directory's denial must be audited");
    assert!(
        denied
            .resource
            .as_ref()
            .unwrap()
            .as_str()
            .ends_with("/secrets")
    );
    assert_eq!(
        denied.outcome,
        AuthorizationOutcome::Denied {
            reason: "secret directory".into()
        }
    );
    let allowed_entry = decided
        .iter()
        .find(|d| d.phase == AuthorizationPhase::DiscoveredResource && d.outcome.is_allowed())
        .expect("the visible entry's allow must be audited too");
    assert!(
        allowed_entry
            .resource
            .as_ref()
            .unwrap()
            .as_str()
            .ends_with("/notes.md")
    );

    // The invocation itself is recorded as succeeded, not denied: the call as a whole was
    // permitted, even though it did not reveal everything on disk.
    let invoked = drain(&mut invocations);
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].outcome, InvocationOutcome::Succeeded);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn denying_the_directory_itself_refuses_the_whole_call_before_any_entry_is_seen() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("locked")).unwrap();
    std::fs::write(root.path().join("locked/inside.txt"), "").unwrap();
    let canonical = root.path().canonicalize().unwrap();

    let policy = engine(json!({ "rules": [
        { "action": "filesystem.list", "resource": format!("{}/locked", canonical.display()),
          "effect": { "decision": "deny", "reason": "locked directory" } },
        { "action": "filesystem.list", "effect": { "decision": "allow" } }
    ]}));

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsListTool::new(root.path()).unwrap())
                .with_policy(Arc::new(policy)),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut decisions = kernel.context().subscribe::<AuthorizationDecided>();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = list_call("locked");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");

    // Refused at the planned-resource phase, before the tool ever ran — so no
    // `DiscoveredResource` decision exists at all for `inside.txt`.
    let decided = drain(&mut decisions);
    assert_eq!(decided.len(), 2);
    assert_eq!(decided[1].phase, AuthorizationPhase::Resource);
    assert!(
        !decided
            .iter()
            .any(|d| d.phase == AuthorizationPhase::DiscoveredResource)
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_policy_configured_denies_listing_even_though_the_root_would_permit_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();

    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_tool(FsListTool::new(root.path()).unwrap()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = list_call("");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_list_tools_confinement_holds_even_when_policy_would_allow_everything() {
    let outer = tempfile::tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsListTool::new(&root_dir).unwrap())
                .with_policy(Arc::new(engine(json!({ "rules": [
                    { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
                    { "action": "*", "effect": { "decision": "allow" } }
                ]})))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let (name, arguments) = list_call("..");
    let error = tools
        .invoke(&name, arguments, &agent("a1"))
        .await
        .unwrap_err();
    // Refused by the tool's own resolution, before any policy question is even asked.
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_to_list_does_not_imply_permission_to_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "secret contents").unwrap();

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(FsListTool::new(root.path()).unwrap())
                .with_tool(FsReadTool::new(root.path()).unwrap())
                .with_policy(Arc::new(engine(json!({ "rules": [
                    { "action": "filesystem.list", "resource": "*", "effect": { "decision": "allow" } },
                    { "action": "filesystem.list", "effect": { "decision": "allow" } }
                ]})))),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    // Listing reveals that `notes.md` exists...
    let (name, arguments) = list_call("");
    let outcome = tools.invoke(&name, arguments, &agent("a1")).await.unwrap();
    assert_eq!(outcome.output["entries"][0]["name"], json!("notes.md"));

    // ...but does not carry any authority to read it: no `filesystem.read` rule exists, so
    // the read is denied even though the exact same file was just visible in a listing.
    let error = tools
        .invoke(
            &ToolName::new(aik_fs::DEFAULT_NAME),
            json!({ "path": "notes.md" }),
            &agent("a1"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PermissionDenied(_)), "{error}");

    kernel.shutdown().await.unwrap();
}
