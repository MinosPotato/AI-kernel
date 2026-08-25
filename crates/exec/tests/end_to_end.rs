//! The vertical slice: `ToolRegistry` (`aik-tools`) + a real `PolicyEngine` (`aik-policy`) +
//! [`ExecTool`], wired as a deployment would wire them.
//!
//! What this proves that the unit tests cannot: that the two resources a call declares are the
//! two a policy is actually asked about, in the form it is asked about them, and that a
//! refusal of either means nothing ran. The last part is the one worth a test of its own — for
//! every other tool in the workspace a denied call means a file was not read, and here it
//! means a program was not started.

use std::sync::Arc;

use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::ErrorKind;
use aik_core::prelude::*;
use aik_exec::{ExecTool, Sandbox};
use aik_policy::RuleBasedPolicyEngine;
use aik_tools::ToolsComponent;
use serde_json::json;

mod support;
use support::agent;

/// Allows the capability and the program, allows one command shape, denies the rest.
fn policy() -> RuleBasedPolicyEngine {
    let document = serde_json::from_value(json!({ "rules": [
        { "action": "process.execute", "resource": "command/cat notes.md",
          "effect": { "decision": "allow" } },
        { "action": "process.execute", "resource": "command/*",
          "effect": { "decision": "deny", "reason": "not an approved command" } },
        { "action": "process.execute", "resource": "program/cat",
          "effect": { "decision": "allow" } },
        { "action": "process.execute", "resource": "program/*",
          "effect": { "decision": "deny", "reason": "not an approved program" } },
        { "action": "process.execute", "effect": { "decision": "allow" } }
    ]}))
    .unwrap();
    RuleBasedPolicyEngine::new(document).unwrap()
}

/// A kernel holding just the registry, the policy and the tool.
async fn kernel(workspace: &std::path::Path) -> Kernel {
    let tool = ExecTool::new(workspace, Sandbox::Unconfined, ["cat", "touch"]).unwrap();
    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_tool(tool)
                .with_policy(Arc::new(policy())),
        )
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    kernel
}

#[tokio::test]
async fn an_allowed_command_runs_through_the_whole_stack() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.md"), "hello, world").unwrap();

    let kernel = kernel(workspace.path()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let outcome = tools
        .invoke(
            &ToolName::new(aik_exec::DEFAULT_NAME),
            json!({ "program": "cat", "arguments": ["notes.md"] }),
            &agent("a1"),
        )
        .await
        .unwrap();

    assert!(!outcome.is_error, "{:?}", outcome.output);
    assert_eq!(outcome.output["stdout"], json!("hello, world"));
    assert_eq!(outcome.output["sandboxed"], json!(false));

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_command_the_policy_refuses_never_starts_a_process() {
    let workspace = tempfile::tempdir().unwrap();
    let kernel = kernel(workspace.path()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    // The program is allowed; this particular command is not. Nothing about `touch` failing
    // to run would be visible in its output, so the file it would have created is the
    // evidence: policy is asked before the process exists.
    let error = tools
        .invoke(
            &ToolName::new(aik_exec::DEFAULT_NAME),
            json!({ "program": "cat", "arguments": ["/etc/passwd"] }),
            &agent("a1"),
        )
        .await
        .expect_err("a denied command is a permission error, not a tool result");

    assert_eq!(error.kind(), ErrorKind::Permission);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_program_the_policy_refuses_is_stopped_before_its_command_is_considered() {
    let workspace = tempfile::tempdir().unwrap();
    let kernel = kernel(workspace.path()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    let error = tools
        .invoke(
            &ToolName::new(aik_exec::DEFAULT_NAME),
            json!({ "program": "touch", "arguments": ["created"] }),
            &agent("a1"),
        )
        .await
        .expect_err("a denied program is a permission error");

    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(
        !workspace.path().join("created").exists(),
        "a refused call must not have run the program"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_program_outside_the_allowlist_is_refused_whatever_the_policy_says() {
    let workspace = tempfile::tempdir().unwrap();
    let kernel = kernel(workspace.path()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    // The policy above ends in a blanket allow of the capability, and says nothing that would
    // stop `sh`. The allowlist is the other limit, and it is not one policy can widen.
    let error = tools
        .invoke(
            &ToolName::new(aik_exec::DEFAULT_NAME),
            json!({ "program": "sh", "arguments": ["-c", "echo hello"] }),
            &agent("a1"),
        )
        .await
        .expect_err("a program that is not registered cannot be reached");

    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_command_is_named_to_policy_exactly_as_it_will_run() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.md"), "hello").unwrap();
    let kernel = kernel(workspace.path()).await;
    let tools = kernel.context().service::<dyn ToolRegistry>().unwrap();

    // `notes.md` is allowed as a single argument. The same characters split differently, or
    // supplied as one argument that merely looks like two, are a different command — which is
    // the whole reason the rendering is injective.
    let error = tools
        .invoke(
            &ToolName::new(aik_exec::DEFAULT_NAME),
            json!({ "program": "cat", "arguments": ["notes.md /etc/passwd"] }),
            &agent("a1"),
        )
        .await
        .expect_err("one argument that contains a space is not two arguments");

    assert_eq!(error.kind(), ErrorKind::Permission);
    kernel.shutdown().await.unwrap();
}
