//! A deterministic, configuration-driven [`PolicyEngine`](aik_api::permission::PolicyEngine).
//!
//! `aik-api::permission` defines the *shape* of authorization — a principal, an action, an
//! optional resource, a [`Decision`](aik_api::permission::Decision) — but deliberately
//! implements nothing: the kernel has no opinion on policy. This crate is one concrete way
//! to answer that question, built from an ordered list of rules read from the kernel's
//! existing [`Config`](aik_core::Config) mechanism.
//!
//! It stays generic on purpose. Nothing here knows what a filesystem path or a shell
//! command is; a rule matches on plain strings ([`Pattern`]), so the exact same mechanism
//! expresses "allow `filesystem.read` under `/home/user/project/`" and "allow
//! `hyprland.dispatch` on `workspace-3`" without either concept being special-cased.
//!
//! # The pieces
//!
//! * [`Pattern`] — a string matcher used for both actions and resources: exact, prefix, or
//!   wildcard.
//! * [`PolicyRule`] — one rule: who ([`PrincipalMatcher`]), what, on what, under what
//!   context, and the [`Decision`](aik_api::permission::Decision) it produces.
//! * [`PolicyDocument`] — an ordered, validated list of rules, the unit configuration is
//!   read as.
//! * [`RuleBasedPolicyEngine`] — the [`PolicyEngine`](aik_api::permission::PolicyEngine)
//!   implementation: first matching rule wins, no match is a denial. See its own
//!   documentation for the full evaluation semantics and why they were chosen.
//!
//! # A complete policy
//!
//! ```
//! use aik_core::config::Config;
//! use aik_policy::RuleBasedPolicyEngine;
//! use serde_json::json;
//!
//! let config = Config::builder()
//!     .layer(json!({
//!         "policy": {
//!             "rules": [
//!                 {
//!                     "principal": { "kind": "agent" },
//!                     "action": "filesystem.read",
//!                     "resource": "/home/user/project/secrets*",
//!                     "effect": { "decision": "deny", "reason": "contains credentials" }
//!                 },
//!                 {
//!                     "action": "filesystem.read",
//!                     "resource": "/home/user/project/*",
//!                     "effect": { "decision": "allow" }
//!                 },
//!                 {
//!                     "action": "filesystem.read",
//!                     "effect": { "decision": "allow" }
//!                 }
//!             ]
//!         }
//!     }))
//!     .build();
//!
//! let engine = RuleBasedPolicyEngine::from_config(&config, "policy").unwrap();
//! assert_eq!(engine.rule_count(), 3);
//! ```
//!
//! Evaluated against that document: a read of `/home/user/project/notes.md` is allowed by
//! the second rule; a read of `/home/user/project/secrets/token` is denied by the first
//! (which is why it is listed before the broader allow); the third rule is what makes the
//! *capability-level* question ("may this principal use `filesystem.read` at all?",
//! carrying no resource) succeed, since neither resource-scoped rule answers it — see
//! [`PolicyRule::resource`].
//!
//! # What this crate does not do
//!
//! It does not enforce anything. Nothing here is invoked automatically; a
//! [`RuleBasedPolicyEngine`] only answers when asked, by whatever holds a
//! `dyn PolicyEngine` — for tools, that is
//! [`ToolRegistry`](aik_api::tool::ToolRegistry), which is also what publishes the audit
//! events for every decision this engine makes. See that trait's documentation for the
//! security reasoning; this crate only supplies rules.

mod document;
mod engine;
mod pattern;
mod rule;

pub use document::PolicyDocument;
pub use engine::RuleBasedPolicyEngine;
pub use pattern::Pattern;
pub use rule::{PolicyRule, PrincipalMatcher};
