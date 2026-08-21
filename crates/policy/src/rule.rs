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
/// Both fields constrain independently (logical AND); omitting a field means "any" for
/// that field. The default matcher — both fields omitted — matches every principal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PrincipalMatcher {
    /// Matched against the principal's id.
    pub id: Pattern,
    /// If present, the principal must be of this kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PrincipalKind>,
}

impl Default for PrincipalMatcher {
    fn default() -> Self {
        Self {
            id: Pattern::Any,
            kind: None,
        }
    }
}

impl PrincipalMatcher {
    fn matches(&self, principal: &aik_api::permission::Principal) -> bool {
        self.id.matches(principal.id.as_str())
            && self.kind.is_none_or(|kind| kind == principal.kind)
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
