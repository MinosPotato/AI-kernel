//! What a sandboxed child can actually reach, checked by running real programs.
//!
//! Every other test in this crate checks what the tool *asks* for: a name validated, an
//! allowlist consulted, a mount plan rendered. Those would all still pass if `bwrap` were
//! invoked with arguments it ignored. These tests check the other end — a process really
//! started, on this host, looking for things it must not find.
//!
//! They are skipped, loudly, where the host cannot provide a sandbox or lacks the handful of
//! ordinary programs they drive. A skipped test here is not a passing one; it means the
//! machine could not answer the question.

use std::path::Path;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::tool::{Tool, ToolOutcome};
use aik_exec::{ExecTool, Sandbox};
use serde_json::{Value, json};

mod support;
use support::{Unasked, agent};

/// The programs these tests drive. All of coreutils, all present on any host with a userland.
const REQUIRED: &[&str] = &["cat", "env", "sleep", "touch", "ls"];

/// Where a program has to be for these tests to find it.
const SEARCH: &[&str] = &["/usr/bin", "/bin"];

/// Whether a program of that name exists on this host.
fn present(name: &str) -> bool {
    SEARCH.iter().any(|dir| Path::new(dir).join(name).exists())
}

/// A sandbox and the programs, or a reason there is nothing to test.
///
/// The distinction this draws is the whole reason it is a function rather than a `?`. A host
/// with no `bwrap` at all cannot answer these questions and is skipped. A host that *has*
/// `bwrap` and cannot start a sandbox with it is a failure, not a skip: that is precisely the
/// state a broken sandbox leaves behind, and a suite that skipped it would go green while
/// every test below silently checked nothing. It did, once — see [`Sandbox::bubblewrap`]'s
/// note on what the probe used to look for.
fn prerequisites() -> Option<Sandbox> {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|name| !present(name))
        .collect();
    if !missing.is_empty() {
        eprintln!("skipped: this host has no {}", missing.join(", "));
        return None;
    }
    if !present("bwrap") {
        eprintln!("skipped: this host has no bubblewrap");
        return None;
    }
    match Sandbox::bubblewrap() {
        Ok(sandbox) => Some(sandbox),
        Err(error) => panic!("bubblewrap is installed but cannot start a sandbox: {error}"),
    }
}

async fn run(tool: &ExecTool, arguments: Value) -> ToolOutcome {
    tool.invoke(arguments, &Unasked, &agent("runner"))
        .await
        .expect("the call itself should not fail")
}

fn text(outcome: &ToolOutcome, stream: &str) -> String {
    outcome.output[stream]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_sandboxed_command_runs_and_reports_what_it_printed() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.md"), "hello, world").unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["cat"]).unwrap();
    let outcome = run(
        &tool,
        json!({ "program": "cat", "arguments": ["notes.md"] }),
    )
    .await;

    assert!(!outcome.is_error, "{:?}", outcome.output);
    assert_eq!(text(&outcome, "stdout"), "hello, world");
    assert_eq!(outcome.output["exit_code"], json!(0));
    assert_eq!(outcome.output["sandboxed"], json!(true));
}

#[tokio::test]
async fn a_sandboxed_command_cannot_see_the_host_filesystem() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("token");
    std::fs::write(&secret, "sk-secret").unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["cat"]).unwrap();
    let outcome = run(
        &tool,
        json!({ "program": "cat", "arguments": [secret.to_string_lossy()] }),
    )
    .await;

    assert!(outcome.is_error);
    assert!(!text(&outcome, "stdout").contains("sk-secret"));
    // `/etc` is not bound either, so the second-most obvious thing to reach for is not there.
    let passwd = run(
        &tool,
        json!({ "program": "cat", "arguments": ["/etc/passwd"] }),
    )
    .await;
    assert!(passwd.is_error, "{:?}", passwd.output);
}

#[tokio::test]
async fn a_read_only_workspace_cannot_be_written_to() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["touch"]).unwrap();
    let outcome = run(
        &tool,
        json!({ "program": "touch", "arguments": ["created"] }),
    )
    .await;

    assert!(outcome.is_error, "{:?}", outcome.output);
    assert!(!workspace.path().join("created").exists());
}

#[tokio::test]
async fn a_writable_workspace_is_the_one_thing_a_child_can_change() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["touch"])
        .unwrap()
        .writable(true);

    let inside = run(
        &tool,
        json!({ "program": "touch", "arguments": ["created"] }),
    )
    .await;
    assert!(!inside.is_error, "{:?}", inside.output);
    assert!(workspace.path().join("created").exists());

    // Writable means the workspace, not the sandbox. `/usr` is still read-only.
    let outside = run(
        &tool,
        json!({ "program": "touch", "arguments": ["/usr/created"] }),
    )
    .await;
    assert!(outside.is_error, "{:?}", outside.output);
}

#[tokio::test]
async fn a_sandboxed_child_has_no_network_unless_one_was_granted() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    // `/proc/net/dev` lists the interfaces the process's network namespace has. In a fresh
    // one that is loopback and nothing else, whatever the host is connected to.
    let confined = ExecTool::new(workspace.path(), sandbox.clone(), ["cat"]).unwrap();
    let outcome = run(
        &confined,
        json!({ "program": "cat", "arguments": ["/proc/net/dev"] }),
    )
    .await;

    assert!(!outcome.is_error, "{:?}", outcome.output);
    let interfaces: Vec<String> = text(&outcome, "stdout")
        .lines()
        .skip(2)
        .filter_map(|line| line.split(':').next().map(|name| name.trim().to_owned()))
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(interfaces, vec!["lo".to_owned()], "{interfaces:?}");

    // And the resolver the host uses is not even visible to a child without a network.
    let resolver = run(
        &confined,
        json!({ "program": "cat", "arguments": ["/etc/resolv.conf"] }),
    )
    .await;
    assert!(resolver.is_error, "{:?}", resolver.output);
}

#[tokio::test]
async fn nothing_of_the_kernel_environment_reaches_a_child() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    // Set on this process, and therefore exactly what must not be inherited.
    // SAFETY: single-threaded at this point in the test, before any child is spawned.
    unsafe {
        std::env::set_var("AIK_EXEC_TEST_SECRET", "sk-secret");
    }

    let tool = ExecTool::new(workspace.path(), sandbox, ["env"])
        .unwrap()
        .with_env("GIT_AUTHOR_NAME", "deployment");
    let outcome = run(&tool, json!({ "program": "env" })).await;

    let printed = text(&outcome, "stdout");
    let mut names: Vec<&str> = printed
        .lines()
        .filter_map(|line| line.split('=').next())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "GIT_AUTHOR_NAME",
            "HOME",
            "LANG",
            "LC_ALL",
            "PATH",
            "PWD",
            "TMPDIR"
        ],
        "{printed}"
    );
    assert!(printed.contains("HOME=/workspace"));
}

#[tokio::test]
async fn a_command_that_does_not_finish_is_killed_and_reported() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["sleep"])
        .unwrap()
        .with_timeout(Duration::from_millis(500));

    let started = std::time::Instant::now();
    let outcome = run(&tool, json!({ "program": "sleep", "arguments": ["30"] })).await;

    assert!(outcome.is_error);
    assert_eq!(outcome.output["timed_out"], json!(true));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call should end with the timeout, not with the command"
    );
}

#[tokio::test]
async fn output_over_the_limit_is_truncated_rather_than_kept() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("big"), "x".repeat(200_000)).unwrap();

    let tool = ExecTool::new(workspace.path(), sandbox, ["cat"])
        .unwrap()
        .with_max_output_bytes(1024);
    let outcome = run(&tool, json!({ "program": "cat", "arguments": ["big"] })).await;

    assert!(!outcome.is_error, "{:?}", outcome.output);
    assert_eq!(text(&outcome, "stdout").len(), 1024);
    assert_eq!(outcome.output["stdout_truncated"], json!(true));
}

#[tokio::test]
async fn standard_input_is_written_when_supplied_and_closed_when_not() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let tool = ExecTool::new(workspace.path(), sandbox, ["cat"]).unwrap();

    let supplied = run(
        &tool,
        json!({ "program": "cat", "stdin": "from the caller" }),
    )
    .await;
    assert_eq!(text(&supplied, "stdout"), "from the caller");

    // Without it the child gets `/dev/null`, so a program that reads standard input sees the
    // end of it immediately rather than blocking on the operator's terminal.
    let absent = run(&tool, json!({ "program": "cat" })).await;
    assert!(!absent.is_error, "{:?}", absent.output);
    assert_eq!(text(&absent, "stdout"), "");
}

#[tokio::test]
async fn a_deadline_that_expires_is_the_callers_failure_rather_than_the_models() {
    let Some(sandbox) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let tool = ExecTool::new(workspace.path(), sandbox, ["sleep"])
        .unwrap()
        .with_timeout(Duration::from_secs(60));

    let cx = ExecutionContext::new()
        .with_principal(Principal::new("runner", PrincipalKind::Agent))
        .with_deadline(aik_core::Timestamp::now().saturating_add(Duration::from_millis(300)));

    let error = tool
        .invoke(
            json!({ "program": "sleep", "arguments": ["30"] }),
            &Unasked,
            &cx,
        )
        .await
        .expect_err("an expired deadline ends the operation, it is not a tool result");
    assert_eq!(error.kind(), aik_core::ErrorKind::Timeout);
}
