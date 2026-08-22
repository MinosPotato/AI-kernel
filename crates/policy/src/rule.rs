//! A single rule and the principal matcher it is built on.

use std::collections::BTreeMap;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Decision, PermissionRequest, PrincipalKind, ResourceId};
use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pattern::Pattern;

/// Matches a [`Principal`](aik_api::permission::Principal).
///
/// Every field constrains independently (logical AND); omitting a field means "any" for
/// that field. The default matcher — every field omitted — matches every principal.
///
/// # Matching delegated authority
///
/// [`on_behalf_of`](PrincipalMatcher::on_behalf_of) is what lets a rule tell one delegate
/// from another, and it exists because without it they are indistinguishable here. Two
/// subsystems mint delegated principals: an agent runs as itself acting for the user who
/// started it, and every scheduled firing runs as a fixed scheduler identity acting for the
/// job's owner. Both therefore reach this matcher under *one* id — `assistant`, `scheduler`
/// — however many people's work they are doing.
///
/// So a rule written as `{ "id": "scheduler", "effect": { "decision": "allow" } }` grants
/// every owner's jobs the same authority, which is almost never what the operator meant.
/// Naming the delegator is what narrows it:
///
/// ```json
/// { "principal": { "id": "scheduler", "on_behalf_of": "alice" },
///   "action": "filesystem.write", "resource": "/home/alice/*",
///   "effect": { "decision": "allow" } }
/// ```
///
/// The field is optional so that every policy written before it existed keeps its meaning
/// exactly. That does mean the *broad* form is still the short one; a rule about delegated
/// work should say whose work it is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PrincipalMatcher {
    /// Matched against the principal's id.
    pub id: Pattern,
    /// If present, the principal must be of this kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PrincipalKind>,
    /// Matched against the principal this one is
    /// [acting for](aik_api::permission::Principal::on_behalf_of).
    ///
    /// Omitting it — the default — matches **any delegation state**, including none, which
    /// is what every rule written before this field existed continues to mean. Present, the
    /// principal must be acting for somebody *and* that somebody must match: so `"*"` reads
    /// "any delegate, nobody autonomous", and `"alice"` reads "whatever is acting for
    /// Alice".
    ///
    /// There is deliberately no way to say "acting for nobody" yet. The question this field
    /// was added to answer is which delegator, and a spelling for the autonomous case should
    /// be chosen when something actually needs to deny autonomous action specifically —
    /// inventing one now would be guessing at a syntax nothing has asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Pattern>,
}

impl Default for PrincipalMatcher {
    fn default() -> Self {
        Self {
            id: Pattern::Any,
            kind: None,
            on_behalf_of: None,
        }
    }
}

impl PrincipalMatcher {
    fn matches(&self, principal: &aik_api::permission::Principal) -> bool {
        self.id.matches(principal.id.as_str())
            && self.kind.is_none_or(|kind| kind == principal.kind)
            && delegation_matches(self.on_behalf_of.as_ref(), principal.on_behalf_of.as_ref())
    }
}

/// Whether a principal's delegation satisfies a matcher's `on_behalf_of`.
///
/// Absent matcher: anything, delegated or not — the backward-compatible reading, and the
/// only one that leaves an existing policy document meaning what it meant. Present matcher:
/// the principal has to be acting for somebody, and that somebody has to match. A principal
/// acting for nobody therefore fails *every* present matcher, `"*"` included, which is what
/// makes `"*"` mean "some delegate" rather than "anyone at all".
fn delegation_matches(
    matcher: Option<&Pattern>,
    on_behalf_of: Option<&aik_api::permission::PrincipalId>,
) -> bool {
    match matcher {
        None => true,
        Some(pattern) => on_behalf_of.is_some_and(|owner| pattern.matches(owner.as_str())),
    }
}

/// One rule in a [`PolicyDocument`](crate::PolicyDocument).
///
/// See [`RuleBasedPolicyEngine`](crate::RuleBasedPolicyEngine) for how rules are combined
/// into a decision; this type only knows how to test whether it applies to one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    /// Which principal this rule applies to. Matches any principal if omitted.
    #[serde(default)]
    pub principal: PrincipalMatcher,

    /// Which action this rule applies to.
    pub action: Pattern,

    /// Which resource this rule applies to.
    ///
    /// Omitting this field entirely means the rule only answers **capability-level**
    /// questions — those with no resource named at all (see
    /// [`ToolSpec::required_permissions`](aik_api::tool::ToolSpec::required_permissions)).
    /// An explicit `"*"` matches capability-level questions *and* any specific resource.
    /// Anything else only matches when a resource is present and matches the pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<Pattern>,

    /// Extra constraints matched against the request's free-form context.
    ///
    /// Each entry is checked against [`PermissionRequest::context`] first, falling back to
    /// [`ExecutionContext::attributes`] if the key is absent there; the rule matches only
    /// if every entry is found and equal to the given value. This is how a rule expresses
    /// "only when this call comes through the `filesystem.read` tool" or "only within this
    /// session" without either axis needing to be a first-class field of the permission
    /// model. Omitted (the default) means no constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<BTreeMap<String, Value>>,

    /// What happens when this rule matches.
    pub effect: Decision,

    /// A human-readable note. Not interpreted; purely for the person reading the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl PolicyRule {
    /// Returns true if every matcher on this rule matches `request` under `cx`.
    pub(crate) fn matches(&self, request: &PermissionRequest, cx: &ExecutionContext) -> bool {
        self.principal.matches(&request.principal)
            && self.action.matches(request.action.as_str())
            && resource_matches(self.resource.as_ref(), request.resource.as_ref())
            && context_matches(self.context.as_ref(), request, cx)
    }

    /// Rejects rules that could never match anything, or effects that carry no useful
    /// information — the configuration equivalent of a typo.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.action.is_vacuous() {
            return Err(Error::config("action", "action pattern must not be empty"));
        }
        if self.principal.id.is_vacuous() {
            return Err(Error::config(
                "principal.id",
                "principal id pattern must not be empty",
            ));
        }
        if let Some(on_behalf_of) = &self.principal.on_behalf_of
            && on_behalf_of.is_vacuous()
        {
            return Err(Error::config(
                "principal.on_behalf_of",
                "principal on_behalf_of pattern must not be empty",
            ));
        }
        if let Some(resource) = &self.resource
            && resource.is_vacuous()
        {
            return Err(Error::config(
                "resource",
                "resource pattern must not be empty",
            ));
        }
        match &self.effect {
            Decision::Deny { reason } if reason.trim().is_empty() => {
                return Err(Error::config(
                    "effect.reason",
                    "deny reason must not be empty",
                ));
            }
            Decision::RequireApproval { prompt } if prompt.trim().is_empty() => {
                return Err(Error::config(
                    "effect.prompt",
                    "approval prompt must not be empty",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

fn resource_matches(pattern: Option<&Pattern>, resource: Option<&ResourceId>) -> bool {
    match (pattern, resource) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(Pattern::Any), _) => true,
        (Some(pattern), Some(resource)) => pattern.matches(resource.as_str()),
        (Some(_), None) => false,
    }
}

fn context_matches(
    constraints: Option<&BTreeMap<String, Value>>,
    request: &PermissionRequest,
    cx: &ExecutionContext,
) -> bool {
    let Some(constraints) = constraints else {
        return true;
    };
    constraints.iter().all(|(key, expected)| {
        let actual = request
            .context
            .get(key.as_str())
            .or_else(|| cx.attributes.get(key.as_str()));
        actual == Some(expected)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{ActionId, Principal};
    use serde_json::json;

    fn request() -> PermissionRequest {
        PermissionRequest {
            principal: Principal::new("agent-1", PrincipalKind::Agent),
            action: ActionId::new("filesystem.read"),
            resource: Some(ResourceId::new("/workspace/notes.md")),
            context: Value::Null,
        }
    }

    #[test]
    fn a_default_rule_matches_any_capability_level_request() {
        let rule = PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse("*"),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        };
        let capability_only = PermissionRequest {
            resource: None,
            ..request()
        };
        assert!(rule.matches(&capability_only, &ExecutionContext::new()));
        // No resource pattern means it does not answer resource-level questions.
        assert!(!rule.matches(&request(), &ExecutionContext::new()));
    }

    #[test]
    fn an_explicit_wildcard_resource_matches_both_levels() {
        let rule = PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse("*"),
            resource: Some(Pattern::Any),
            context: None,
            effect: Decision::Allow,
            description: None,
        };
        assert!(rule.matches(
            &PermissionRequest {
                resource: None,
                ..request()
            },
            &ExecutionContext::new()
        ));
        assert!(rule.matches(&request(), &ExecutionContext::new()));
    }

    #[test]
    fn principal_kind_and_id_both_constrain() {
        let rule = PolicyRule {
            principal: PrincipalMatcher {
                id: Pattern::parse("agent-1"),
                kind: Some(PrincipalKind::Agent),
                on_behalf_of: None,
            },
            action: Pattern::parse("*"),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        };
        let wrong_kind = PermissionRequest {
            principal: Principal::new("agent-1", PrincipalKind::User),
            resource: None,
            ..request()
        };
        assert!(!rule.matches(&wrong_kind, &ExecutionContext::new()));

        let wrong_id = PermissionRequest {
            principal: Principal::new("agent-2", PrincipalKind::Agent),
            resource: None,
            ..request()
        };
        assert!(!rule.matches(&wrong_id, &ExecutionContext::new()));
    }

    #[test]
    fn context_constraints_check_request_then_execution_context() {
        let rule = PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse("*"),
            resource: None,
            context: Some(BTreeMap::from([("tool".to_owned(), json!("fs.read"))])),
            effect: Decision::Allow,
            description: None,
        };

        let via_request_context = PermissionRequest {
            resource: None,
            context: json!({ "tool": "fs.read" }),
            ..request()
        };
        assert!(rule.matches(&via_request_context, &ExecutionContext::new()));

        let via_execution_context =
            ExecutionContext::new().with_attribute("tool", json!("fs.read"));
        let no_request_context = PermissionRequest {
            resource: None,
            ..request()
        };
        assert!(rule.matches(&no_request_context, &via_execution_context));

        let mismatched = PermissionRequest {
            resource: None,
            context: json!({ "tool": "fs.write" }),
            ..request()
        };
        assert!(!rule.matches(&mismatched, &ExecutionContext::new()));
    }

    #[test]
    fn empty_patterns_fail_validation() {
        let mut rule = PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse(""),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        };
        assert!(rule.validate().is_err());

        rule.action = Pattern::parse("*");
        rule.resource = Some(Pattern::parse(""));
        assert!(rule.validate().is_err());

        rule.resource = None;
        rule.principal.id = Pattern::parse("");
        assert!(rule.validate().is_err());
    }

    fn matcher(json: serde_json::Value) -> PrincipalMatcher {
        serde_json::from_value(json).expect("a valid matcher")
    }

    fn delegate(id: &str, owner: Option<&str>) -> aik_api::permission::Principal {
        let principal = aik_api::permission::Principal::new(id, PrincipalKind::System);
        match owner {
            Some(owner) => principal.on_behalf_of(owner),
            None => principal,
        }
    }

    #[test]
    fn an_omitted_on_behalf_of_matches_every_delegation_state() {
        // The backward-compatibility guarantee: a document written before this field existed
        // still selects exactly the principals it always did.
        let any = matcher(serde_json::json!({ "id": "scheduler" }));
        assert_eq!(any.on_behalf_of, None);
        assert!(any.matches(&delegate("scheduler", None)));
        assert!(any.matches(&delegate("scheduler", Some("alice"))));
        assert!(any.matches(&delegate("scheduler", Some("bob"))));
    }

    #[test]
    fn a_named_on_behalf_of_matches_that_delegator_and_nobody_else() {
        let for_alice = matcher(serde_json::json!({ "id": "scheduler", "on_behalf_of": "alice" }));
        assert!(for_alice.matches(&delegate("scheduler", Some("alice"))));
        assert!(!for_alice.matches(&delegate("scheduler", Some("bob"))));
        assert!(
            !for_alice.matches(&delegate("scheduler", None)),
            "a principal acting for nobody is not acting for alice"
        );
    }

    #[test]
    fn a_wildcard_on_behalf_of_means_some_delegate_rather_than_anyone() {
        let any_delegate = matcher(serde_json::json!({ "id": "scheduler", "on_behalf_of": "*" }));
        assert!(any_delegate.matches(&delegate("scheduler", Some("alice"))));
        assert!(any_delegate.matches(&delegate("scheduler", Some("bob"))));
        assert!(
            !any_delegate.matches(&delegate("scheduler", None)),
            "`*` here separates delegated work from autonomous work; matching both would \
             make the field unable to express the only distinction it was added for"
        );
    }

    #[test]
    fn on_behalf_of_matches_by_prefix_like_every_other_axis() {
        let team = matcher(serde_json::json!({ "id": "*", "on_behalf_of": "team-*" }));
        assert!(team.matches(&delegate("scheduler", Some("team-ops"))));
        assert!(!team.matches(&delegate("scheduler", Some("teamster"))));
    }

    #[test]
    fn the_delegation_matcher_composes_with_the_other_axes_rather_than_replacing_them() {
        let matcher = matcher(serde_json::json!({
            "id": "scheduler", "kind": "system", "on_behalf_of": "alice"
        }));
        assert!(matcher.matches(&delegate("scheduler", Some("alice"))));
        assert!(
            !matcher.matches(&delegate("agent", Some("alice"))),
            "id still constrains"
        );
        assert!(
            !matcher.matches(
                &aik_api::permission::Principal::new("scheduler", PrincipalKind::Agent)
                    .on_behalf_of("alice")
            ),
            "kind still constrains"
        );
    }

    #[test]
    fn an_empty_on_behalf_of_pattern_fails_validation() {
        // The same typo guard the other axes have: an empty string parses as an exact match
        // on nothing, which can only ever be a mistake in a document.
        let rule = PolicyRule {
            principal: PrincipalMatcher {
                id: Pattern::parse("*"),
                kind: None,
                on_behalf_of: Some(Pattern::parse("")),
            },
            action: Pattern::parse("*"),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        };
        let error = rule.validate().unwrap_err();
        assert!(
            error.to_string().contains("on_behalf_of"),
            "the failure has to name the field that is wrong: {error}"
        );
    }

    #[test]
    fn a_matcher_round_trips_through_configuration() {
        let matcher = matcher(serde_json::json!({ "id": "scheduler", "on_behalf_of": "alice" }));
        let json = serde_json::to_value(&matcher).unwrap();
        assert_eq!(json["on_behalf_of"], serde_json::json!("alice"));
        assert_eq!(
            serde_json::from_value::<PrincipalMatcher>(json).unwrap(),
            matcher
        );

        let plain = PrincipalMatcher::default();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(
            json.get("on_behalf_of").is_none(),
            "an unset delegation matcher is absent rather than null, so a round trip cannot \
             turn `any` into something narrower"
        );
    }

    #[test]
    fn blank_deny_reasons_and_approval_prompts_fail_validation() {
        let base = PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse("*"),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        };

        let mut denied = base.clone();
        denied.effect = Decision::deny("   ");
        assert!(denied.validate().is_err());

        let mut asked = base;
        asked.effect = Decision::ask("");
        assert!(asked.validate().is_err());
    }
}
