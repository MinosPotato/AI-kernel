//! [`RuleBasedPolicyEngine`]: a deterministic, configuration-driven [`PolicyEngine`].

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Decision, PermissionRequest, PolicyEngine};
use aik_core::Result;
use aik_core::config::Config;
use async_trait::async_trait;

use crate::document::PolicyDocument;
use crate::rule::PolicyRule;

/// Decides permission requests by trying an ordered list of rules and taking the first
/// match.
///
/// # Evaluation semantics
///
/// Rules are tried in the order they appear in the document. The **first rule whose
/// principal, action, resource and context matchers all match** decides the request;
/// every rule after it is not even consulted, whether or not it would also have matched.
/// If no rule matches at all, the request is denied — an absent rule is never a silent
/// allow.
///
/// This is a deliberately simple, total order: no computed "specificity", no combining
/// multiple matching rules, no separate notion of an overriding "explicit deny". It has
/// one practical consequence policy authors must know: **write more specific rules before
/// the general rules they are meant to carve exceptions out of.** For example, to allow
/// reading a project directory but deny one file inside it:
///
/// ```json
/// { "rules": [
///     { "action": "filesystem.read",
///       "resource": "/home/user/project/secrets*",
///       "effect": { "decision": "deny", "reason": "contains credentials" } },
///     { "action": "filesystem.read",
///       "resource": "/home/user/project/*",
///       "effect": { "decision": "allow" } }
/// ]}
/// ```
///
/// If the two rules were swapped, the general `allow` would match first and the `deny`
/// would never be reached — the engine does not reorder rules by specificity on your
/// behalf, because doing so would mean inventing a precedence model this crate does not
/// document elsewhere. First-match-wins is the whole model.
///
/// # Fail-closed
///
/// No document, an empty one, and a document with no matching rule are all denials, never
/// an allow (see [`PolicyDocument`]). Because only the first match is ever taken, there is
/// no "ambiguous" outcome at evaluation time either: exactly one rule, or none, decides
/// any given request.
///
/// # Concurrency and isolation
///
/// An engine is immutable after construction — evaluating it never mutates anything — so
/// concurrent calls to [`PolicyEngine::evaluate`] need no synchronisation, and two engines
/// built from different documents share no state whatsoever.
#[derive(Debug, Clone)]
pub struct RuleBasedPolicyEngine {
    rules: Arc<[PolicyRule]>,
}

impl RuleBasedPolicyEngine {
    /// Builds an engine from an already-parsed document, after validating it.
    pub fn new(document: PolicyDocument) -> Result<Self> {
        document.validate()?;
        Ok(Self {
            rules: document.rules.into(),
        })
    }

    /// Reads and validates a document from `config` at `path`, defaulting to an empty
    /// (deny-everything) document if nothing is configured there.
    ///
    /// This uses the kernel's existing [`Config`] mechanism rather than a policy-specific
    /// file format: a policy document is just JSON, like everything else the kernel
    /// configures, so the host decides whether it originates from a file on disk, an
    /// environment variable, or a value built in code. See [`Config`] for how layers are
    /// assembled; this crate has no opinion on that.
    pub fn from_config(config: &Config, path: &str) -> Result<Self> {
        let document: PolicyDocument = config.get_or_default(path)?;
        Self::new(document)
    }

    /// How many rules are loaded. Mostly useful for diagnostics and tests.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

#[async_trait]
impl PolicyEngine for RuleBasedPolicyEngine {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        cx: &ExecutionContext,
    ) -> Result<Decision> {
        for rule in self.rules.iter() {
            if rule.matches(request, cx) {
                return Ok(rule.effect.clone());
            }
        }
        Ok(Decision::deny("no policy rule matched this request"))
    }
}
