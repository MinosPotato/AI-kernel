//! End-to-end tests for [`RuleBasedPolicyEngine`]: what a document expresses, how
//! conflicting rules resolve, and the fail-closed guarantees the rest of the kernel relies
//! on.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{
    ActionId, Decision, PermissionRequest, PolicyEngine, Principal, PrincipalKind, ResourceId,
};
use aik_core::config::Config;
use aik_core::{Error, ErrorKind};
use aik_policy::{PolicyDocument, RuleBasedPolicyEngine};
use serde_json::json;

fn request(principal: Principal, action: &str, resource: Option<&str>) -> PermissionRequest {
    PermissionRequest {
        principal,
        action: ActionId::new(action),
        resource: resource.map(ResourceId::new),
        context: serde_json::Value::Null,
    }
}

fn agent(id: &str) -> Principal {
    Principal::new(id, PrincipalKind::Agent)
}

fn engine_from(document: serde_json::Value) -> RuleBasedPolicyEngine {
    let config = Config::builder()
        .layer(json!({ "policy": document }))
        .build();
    RuleBasedPolicyEngine::from_config(&config, "policy").unwrap()
}

// ---------------------------------------------------------------------------
// Explicit allow / deny
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_allow_rule_allows() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ]}));

    let decision = engine
        .evaluate(
            &request(agent("a1"), "filesystem.read", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(decision, Decision::Allow);
}

#[tokio::test]
async fn an_explicit_deny_rule_denies() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read",
          "effect": { "decision": "deny", "reason": "disabled for now" } }
    ]}));

    let decision = engine
        .evaluate(
            &request(agent("a1"), "filesystem.read", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(decision, Decision::deny("disabled for now"));
}

// ---------------------------------------------------------------------------
// Resource-specific rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resource_specific_allow_only_matches_the_named_prefix() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "resource": "/home/user/project/*",
          "effect": { "decision": "allow" } }
    ]}));

    let inside = request(
        agent("a1"),
        "filesystem.read",
        Some("/home/user/project/notes.md"),
    );
    assert_eq!(
        engine
            .evaluate(&inside, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );

    let outside = request(agent("a1"), "filesystem.read", Some("/etc/shadow"));
    assert!(
        matches!(
            engine
                .evaluate(&outside, &ExecutionContext::new())
                .await
                .unwrap(),
            Decision::Deny { .. }
        ),
        "a resource outside the declared prefix must not match"
    );
}

#[tokio::test]
async fn resource_specific_deny_overrides_when_listed_first() {
    // The documented convention: specific rules must precede the general rules they carve
    // exceptions out of, because evaluation is first-match-wins.
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
          "effect": { "decision": "deny", "reason": "contains credentials" } },
        { "action": "filesystem.read", "resource": "/home/user/project/*",
          "effect": { "decision": "allow" } }
    ]}));

    let secret = request(
        agent("a1"),
        "filesystem.read",
        Some("/home/user/project/secrets/token"),
    );
    assert_eq!(
        engine
            .evaluate(&secret, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::deny("contains credentials")
    );

    let ordinary = request(
        agent("a1"),
        "filesystem.read",
        Some("/home/user/project/readme.md"),
    );
    assert_eq!(
        engine
            .evaluate(&ordinary, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );
}

// ---------------------------------------------------------------------------
// Overlapping rules: order, not specificity, decides
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_matching_rule_wins_even_when_a_later_rule_would_also_match() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "effect": { "decision": "allow" } },
        { "action": "filesystem.read",
          "effect": { "decision": "deny", "reason": "unreachable" } }
    ]}));

    let decision = engine
        .evaluate(
            &request(agent("a1"), "filesystem.read", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(decision, Decision::Allow);
}

#[tokio::test]
async fn reversing_specific_and_general_rules_changes_the_outcome() {
    // The mirror image of `resource_specific_deny_overrides_when_listed_first`: this is
    // not a bug, it is the documented, deliberately simple evaluation model — there is no
    // hidden specificity ordering rescuing a badly-ordered document.
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "resource": "/home/user/project/*",
          "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
          "effect": { "decision": "deny", "reason": "contains credentials" } }
    ]}));

    let secret = request(
        agent("a1"),
        "filesystem.read",
        Some("/home/user/project/secrets/token"),
    );
    assert_eq!(
        engine
            .evaluate(&secret, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow,
        "the general allow matched first, so the more specific deny is never reached"
    );
}

// ---------------------------------------------------------------------------
// Missing rules / fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_matching_no_rule_is_denied() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ]}));

    let decision = engine
        .evaluate(
            &request(agent("a1"), "filesystem.write", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, Decision::Deny { .. }));
}

#[tokio::test]
async fn an_empty_document_denies_everything() {
    let engine = RuleBasedPolicyEngine::new(PolicyDocument::empty()).unwrap();
    let decision = engine
        .evaluate(
            &request(agent("a1"), "anything.at.all", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, Decision::Deny { .. }));
}

#[tokio::test]
async fn a_config_section_that_is_entirely_absent_still_denies() {
    let config = Config::empty();
    let engine = RuleBasedPolicyEngine::from_config(&config, "policy").unwrap();
    assert_eq!(engine.rule_count(), 0);
    let decision = engine
        .evaluate(
            &request(agent("a1"), "anything", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, Decision::Deny { .. }));
}

// ---------------------------------------------------------------------------
// Malformed configuration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wrong_json_shape_is_a_config_error() {
    let config = Config::builder()
        .layer(json!({ "policy": { "rules": "not a list" } }))
        .build();
    let error = RuleBasedPolicyEngine::from_config(&config, "policy").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}

#[tokio::test]
async fn unknown_fields_are_a_config_error() {
    let config = Config::builder()
        .layer(json!({ "policy": { "rules": [
            { "action": "fs.read", "effect": { "decision": "allow" }, "typo_field": true }
        ]}}))
        .build();
    let error = RuleBasedPolicyEngine::from_config(&config, "policy").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}

#[tokio::test]
async fn an_empty_action_pattern_fails_validation() {
    let document: PolicyDocument = serde_json::from_value(json!({ "rules": [
        { "action": "", "effect": { "decision": "allow" } }
    ]}))
    .unwrap();
    let error = RuleBasedPolicyEngine::new(document).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
    assert!(error.to_string().contains("rules[0]"), "{error}");
}

#[tokio::test]
async fn a_blank_deny_reason_fails_validation() {
    let document: PolicyDocument = serde_json::from_value(json!({ "rules": [
        { "action": "*", "effect": { "decision": "deny", "reason": "" } }
    ]}))
    .unwrap();
    assert!(matches!(
        RuleBasedPolicyEngine::new(document),
        Err(Error::Config { .. })
    ));
}

// ---------------------------------------------------------------------------
// RequireApproval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn require_approval_rules_are_returned_unresolved() {
    // The engine only decides policy; it never talks to an approval sink itself — that is
    // `InProcessToolRegistry`'s job (see `aik-tools`). This proves the engine hands the
    // decision back verbatim rather than trying to resolve it.
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.write",
          "effect": { "decision": "require_approval", "prompt": "allow writing?" } }
    ]}));

    let decision = engine
        .evaluate(
            &request(agent("a1"), "filesystem.write", None),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(decision, Decision::ask("allow writing?"));
}

// ---------------------------------------------------------------------------
// Multiple principals / resources / actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn different_principals_get_different_answers() {
    let engine = engine_from(json!({ "rules": [
        { "principal": { "id": "trusted-agent" }, "action": "filesystem.write",
          "effect": { "decision": "allow" } },
        { "action": "filesystem.write",
          "effect": { "decision": "deny", "reason": "not a trusted principal" } }
    ]}));

    let trusted = request(agent("trusted-agent"), "filesystem.write", None);
    assert_eq!(
        engine
            .evaluate(&trusted, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );

    let untrusted = request(agent("random-agent"), "filesystem.write", None);
    assert!(matches!(
        engine
            .evaluate(&untrusted, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Deny { .. }
    ));
}

#[tokio::test]
async fn principal_kind_alone_can_distinguish_users_from_agents() {
    let engine = engine_from(json!({ "rules": [
        { "principal": { "kind": "user" }, "action": "filesystem.write",
          "effect": { "decision": "allow" } },
        { "principal": { "kind": "agent" }, "action": "filesystem.write",
          "effect": { "decision": "require_approval", "prompt": "an agent wants to write" } }
    ]}));

    let by_user = request(
        Principal::new("u1", PrincipalKind::User),
        "filesystem.write",
        None,
    );
    assert_eq!(
        engine
            .evaluate(&by_user, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );

    let by_agent = request(agent("a1"), "filesystem.write", None);
    assert!(matches!(
        engine
            .evaluate(&by_agent, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::RequireApproval { .. }
    ));
}

#[tokio::test]
async fn three_resources_under_one_rule_set_get_three_different_answers() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "resource": "/workspace/secrets*",
          "effect": { "decision": "deny", "reason": "secret" } },
        { "action": "filesystem.read", "resource": "/workspace/*",
          "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "resource": "*",
          "effect": { "decision": "deny", "reason": "outside the workspace" } }
    ]}));

    let cases = [
        ("/workspace/notes.md", true),
        ("/workspace/secrets/token", false),
        ("/etc/shadow", false),
    ];
    for (resource, expect_allow) in cases {
        let decision = engine
            .evaluate(
                &request(agent("a1"), "filesystem.read", Some(resource)),
                &ExecutionContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision.is_allowed(), expect_allow, "resource `{resource}`");
    }
}

#[tokio::test]
async fn multiple_independent_actions_are_governed_separately() {
    let engine = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "effect": { "decision": "allow" } },
        { "action": "filesystem.write",
          "effect": { "decision": "deny", "reason": "read-only deployment" } },
        { "action": "process.exec",
          "effect": { "decision": "require_approval", "prompt": "run a process?" } }
    ]}));

    assert_eq!(
        engine
            .evaluate(
                &request(agent("a1"), "filesystem.read", None),
                &ExecutionContext::new()
            )
            .await
            .unwrap(),
        Decision::Allow
    );
    assert!(matches!(
        engine
            .evaluate(
                &request(agent("a1"), "filesystem.write", None),
                &ExecutionContext::new()
            )
            .await
            .unwrap(),
        Decision::Deny { .. }
    ));
    assert!(matches!(
        engine
            .evaluate(
                &request(agent("a1"), "process.exec", None),
                &ExecutionContext::new()
            )
            .await
            .unwrap(),
        Decision::RequireApproval { .. }
    ));
}

// ---------------------------------------------------------------------------
// Concurrency and isolation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_evaluations_against_one_engine_are_all_correct() {
    let engine = Arc::new(engine_from(json!({ "rules": [
        { "action": "filesystem.read", "resource": "/workspace/*",
          "effect": { "decision": "allow" } },
        { "action": "filesystem.read", "resource": "*",
          "effect": { "decision": "deny", "reason": "outside the workspace" } }
    ]})));

    let mut handles = Vec::new();
    for i in 0..200 {
        let engine = engine.clone();
        let (resource, expect_allow) = if i % 2 == 0 {
            (format!("/workspace/file-{i}"), true)
        } else {
            (format!("/other/file-{i}"), false)
        };
        handles.push(tokio::spawn(async move {
            let decision = engine
                .evaluate(
                    &request(agent("a1"), "filesystem.read", Some(&resource)),
                    &ExecutionContext::new(),
                )
                .await
                .unwrap();
            assert_eq!(decision.is_allowed(), expect_allow, "resource `{resource}`");
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn two_engines_built_from_different_documents_do_not_affect_each_other() {
    let permissive = engine_from(json!({ "rules": [
        { "action": "filesystem.read", "effect": { "decision": "allow" } }
    ]}));
    let restrictive = engine_from(json!({ "rules": [
        { "action": "filesystem.read",
          "effect": { "decision": "deny", "reason": "locked down" } }
    ]}));

    let req = request(agent("a1"), "filesystem.read", None);
    assert_eq!(
        permissive
            .evaluate(&req, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );
    assert_eq!(
        restrictive
            .evaluate(&req, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::deny("locked down")
    );

    // Re-checking the first engine proves evaluating the second never mutated shared state.
    assert_eq!(
        permissive
            .evaluate(&req, &ExecutionContext::new())
            .await
            .unwrap(),
        Decision::Allow
    );
}
