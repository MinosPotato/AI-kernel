//! What the shipped policy actually decides about running programs.
//!
//! `aik.example.json` is the file the documentation tells people to start from, so its rules
//! are not an illustration: they are the default answer to "may this agent run this command",
//! on every deployment that copies it. Two things about them are easy to get wrong and
//! invisible when wrong:
//!
//! * **Prefix patterns.** A `resource` ending in `*` matches by prefix, with no word boundary.
//!   `command/git*` therefore also matches `gitk`, and `program/c*` matches `curl`. Every
//!   pattern in the shipped file is written with its separating space included for that
//!   reason, and this file is what notices if one loses it.
//! * **Two questions, not one.** A call declares the program *and* the whole command, so a
//!   rule allowing `program/rg` says nothing about `rg --files`. A policy that allowed the
//!   program and forgot the command would put every single call to a human, which reads as
//!   the tool being broken rather than as a rule being incomplete.

use std::path::Path;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind, ResourceId,
};
use aik_policy::RuleBasedPolicyEngine;

/// The policy document the repository ships, as a live engine.
fn shipped() -> RuleBasedPolicyEngine {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cli")
        .join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("the example configuration is readable");
    let file: serde_json::Value =
        serde_json::from_str(&text).expect("the example configuration is valid JSON");
    let document = serde_json::from_value(file["policy"].clone()).expect("a policy document");
    RuleBasedPolicyEngine::new(document).expect("the shipped policy is valid")
}

/// What the shipped policy says about one resource.
async fn decide(engine: &RuleBasedPolicyEngine, resource: &str) -> Decision {
    let request = PermissionRequest {
        principal: Principal::new("assistant", PrincipalKind::Agent).on_behalf_of("user"),
        action: ActionId::new(aik_exec::DEFAULT_PERMISSION),
        resource: Some(ResourceId::new(resource)),
        context: serde_json::Value::Null,
    };
    engine
        .evaluate(&request, &ExecutionContext::new())
        .await
        .expect("the engine answers")
}

fn is_allow(decision: &Decision) -> bool {
    matches!(decision, Decision::Allow)
}

fn is_approval(decision: &Decision) -> bool {
    matches!(decision, Decision::RequireApproval { .. })
}

fn is_deny(decision: &Decision) -> bool {
    matches!(decision, Decision::Deny { .. })
}

#[tokio::test]
async fn reading_the_repository_is_allowed_without_asking_anybody() {
    let engine = shipped();
    for resource in [
        "command/git status",
        "command/git status --short",
        "command/git log --oneline -n 5",
        "command/git diff --stat",
        "command/rg --files",
        "command/ls",
        "command/ls -la src",
        "command/cat README.md",
        "command/wc -l src/lib.rs",
    ] {
        let decision = decide(&engine, resource).await;
        assert!(is_allow(&decision), "{resource}: {decision:?}");
    }
}

#[tokio::test]
async fn anything_git_can_do_beyond_reading_goes_to_a_human() {
    let engine = shipped();
    for resource in [
        "command/git push origin main",
        "command/git config --global core.pager cat",
        "command/git commit -m 'a message'",
        "command/git checkout main",
        "command/gitk",
    ] {
        let decision = decide(&engine, resource).await;
        assert!(is_approval(&decision), "{resource}: {decision:?}");
    }
}

#[tokio::test]
async fn an_allowed_subcommand_is_allowed_with_any_arguments() {
    // Stated as a test because it is a real property of the shipped rules rather than an
    // accident: `command/git log *` is a prefix, so every flag `git log` accepts is allowed,
    // including ones that can make git run an external program (`--ext-diff` with a configured
    // driver). A pattern language of prefixes cannot express "these arguments and no others",
    // and pretending otherwise by enumerating flags would give a false sense of a boundary.
    //
    // What actually bounds this is the sandbox: whatever `git log` starts, starts inside the
    // same namespaces, with no network and a read-only workspace. A deployment that wants
    // argument-level control puts `command/git log *` behind `require_approval` instead, where
    // the human is shown the whole command.
    let engine = shipped();
    let decision = decide(&engine, "command/git log --ext-diff").await;
    assert!(is_allow(&decision), "{decision:?}");
}

#[tokio::test]
async fn a_program_the_policy_never_named_is_denied_rather_than_asked_about() {
    let engine = shipped();
    for resource in [
        "program/curl",
        "program/sh",
        "program/bash",
        "program/python3",
    ] {
        let decision = decide(&engine, resource).await;
        assert!(is_deny(&decision), "{resource}: {decision:?}");
    }
}

#[tokio::test]
async fn a_prefix_pattern_does_not_reach_past_the_name_it_names() {
    let engine = shipped();

    // Every one of these is a real program whose name extends an allowed one. If a rule in the
    // shipped file ever loses its separating space, one of these becomes allowed silently.
    for resource in [
        "program/gitk",
        "program/lsof",
        "program/catman",
        "program/rgb",
    ] {
        let decision = decide(&engine, resource).await;
        assert!(is_deny(&decision), "{resource}: {decision:?}");
    }
    for resource in ["command/gitk", "command/lsof -i", "command/catman -w"] {
        let decision = decide(&engine, resource).await;
        assert!(
            !is_allow(&decision),
            "{resource} must not be allowed by a rule meant for another program: {decision:?}"
        );
    }
}

#[tokio::test]
async fn the_allowed_programs_are_allowed_as_programs() {
    let engine = shipped();
    for resource in [
        "program/git",
        "program/rg",
        "program/ls",
        "program/cat",
        "program/wc",
    ] {
        let decision = decide(&engine, resource).await;
        assert!(is_allow(&decision), "{resource}: {decision:?}");
    }
}

#[tokio::test]
async fn every_program_the_deployment_lists_can_actually_run_something() {
    // The two settings that have to agree: `agent.exec.programs` says what the tool will
    // accept, and the policy says what it will allow. A program in the first and absent from
    // the second is a deployment where a listed capability is dead, which reads to a user as
    // the tool being broken.
    let engine = shipped();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cli")
        .join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("readable");
    let file: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

    let programs = file["agent"]["exec"]["programs"]
        .as_array()
        .expect("the shipped deployment names its programs");
    assert!(!programs.is_empty());

    for program in programs {
        let name = program.as_str().expect("a program name");
        let decision = decide(&engine, &format!("program/{name}")).await;
        assert!(
            is_allow(&decision),
            "`{name}` is listed but the policy refuses it: {decision:?}"
        );

        let with_arguments = decide(&engine, &format!("command/{name} --version")).await;
        assert!(
            !is_deny(&with_arguments),
            "`{name}` is listed but no command with it is reachable: {with_arguments:?}"
        );
    }
}
