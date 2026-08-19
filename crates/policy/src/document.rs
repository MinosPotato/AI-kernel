//! A policy document: an ordered, validated list of rules.

use aik_core::Result;
use serde::{Deserialize, Serialize};

use crate::rule::PolicyRule;

/// An ordered list of [`PolicyRule`]s, as read from configuration.
///
/// Order is significant — see [`RuleBasedPolicyEngine`](crate::RuleBasedPolicyEngine) for
/// how it is used. An absent or empty document is valid and denies everything, since a
/// [`RuleBasedPolicyEngine`](crate::RuleBasedPolicyEngine) with no rules never finds a
/// match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PolicyDocument {
    /// The rules, tried in order.
    pub rules: Vec<PolicyRule>,
}

impl PolicyDocument {
    /// An empty document: no rules, everything denied.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Checks every rule for the kind of mistake deserialisation cannot catch on its own —
    /// an empty pattern, a blank deny reason, a blank approval prompt. Unknown fields and
    /// type mismatches are already rejected by [`serde`] itself before this runs.
    pub fn validate(&self) -> Result<()> {
        for (index, rule) in self.rules.iter().enumerate() {
            rule.validate()
                .map_err(|error| prefix_path(error, &format!("rules[{index}]")))?;
        }
        Ok(())
    }
}

/// Rewrites a config error's path to include which rule it came from.
fn prefix_path(error: aik_core::Error, prefix: &str) -> aik_core::Error {
    match error {
        aik_core::Error::Config { path, message } => aik_core::Error::config(
            if path.is_empty() {
                prefix.to_owned()
            } else {
                format!("{prefix}.{path}")
            },
            message,
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::Pattern;
    use crate::rule::PrincipalMatcher;
    use aik_api::permission::Decision;

    fn rule(action: &str) -> PolicyRule {
        PolicyRule {
            principal: PrincipalMatcher::default(),
            action: Pattern::parse(action),
            resource: None,
            context: None,
            effect: Decision::Allow,
            description: None,
        }
    }

    #[test]
    fn an_empty_document_is_valid() {
        assert!(PolicyDocument::empty().validate().is_ok());
    }

    #[test]
    fn a_bad_rule_is_named_by_index() {
        let document = PolicyDocument {
            rules: vec![rule("fs.read"), rule("")],
        };
        let error = document.validate().unwrap_err();
        assert!(error.to_string().contains("rules[1]"), "{error}");
    }

    #[test]
    fn documents_deserialise_from_json() {
        let document: PolicyDocument = serde_json::from_value(serde_json::json!({
            "rules": [
                { "action": "fs.read", "effect": { "decision": "allow" } }
            ]
        }))
        .unwrap();
        assert_eq!(document.rules.len(), 1);
        assert!(document.validate().is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let result: std::result::Result<PolicyDocument, _> =
            serde_json::from_value(serde_json::json!({ "rulez": [] }));
        assert!(result.is_err());
    }
}
