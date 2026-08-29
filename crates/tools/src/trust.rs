//! What a conversation has read, and what the registry does about it.
//!
//! The contracts are in [`aik_api::provenance`]; this is the in-process implementation of
//! the ledger, and the deployment-level switch that says how strictly
//! [`InProcessToolRegistry`](crate::InProcessToolRegistry) enforces it.

use std::collections::HashSet;
use std::sync::Mutex;

use aik_api::provenance::{Trust, TrustLedger, TrustScope};
use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The action recorded for the question "may untrusted content act through this tool?".
///
/// It is not a [`ToolSpec::required_permissions`](aik_api::tool::ToolSpec::required_permissions)
/// entry and no policy rule is evaluated against it: the question is decided by
/// [`TrustEnforcement`], not by the policy engine. The name exists so that the decision has
/// one in the audit trail, and so an operator reading the trail can grep for it.
pub const UNTRUSTED_CONTENT_ACTION: &str = "aik.untrusted-content";

/// How strictly a registry enforces provenance.
///
/// This is the one deployment-level dial on the mechanism. It applies only where all three
/// of the following hold, which is the case the mechanism exists for: the conversation has
/// read untrusted content, the tool being asked for is not
/// [`Reach::Contained`](aik_api::provenance::Reach::Contained), and policy has *already*
/// allowed the call. Nothing here can turn a denial into an allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustEnforcement {
    /// Ask a human, and refuse if there is nobody to ask.
    ///
    /// The default, and the reason the default is not [`TrustEnforcement::Deny`]: reading a
    /// file and then writing one is most of what an assistant is for, so refusing it outright
    /// would make the ordinary case impossible, while asking makes the *unordinary* one —
    /// "this conversation fetched a page, and now wants to write to your home directory" —
    /// visible at the moment it matters. An unattended deployment has no sink, so this is
    /// still a refusal wherever there is nobody watching.
    #[default]
    Approval,
    /// Refuse outright.
    ///
    /// For a deployment that runs unattended and would rather a job fail than wait for
    /// somebody who is not there, and for one where the answer to "should tainted content be
    /// able to do this?" is simply no.
    Deny,
    /// Allow, and record it.
    ///
    /// The escape hatch, and deliberately the only one: no per-tool exemptions, no
    /// per-principal ones. Provenance is either enforced in a deployment or it is not, and a
    /// list of exceptions is how a boundary becomes a suggestion. The audit trail still
    /// carries every [`AuthorizationPhase::Trust`](aik_api::audit::AuthorizationPhase::Trust)
    /// decision and the [`Trust`] of every tool result, so this is "observe" rather than
    /// "off".
    Observe,
}

impl TrustEnforcement {
    /// The value used in configuration and in a log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Deny => "deny",
            Self::Observe => "observe",
        }
    }
}

impl std::fmt::Display for TrustEnforcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many tainted scopes an [`InMemoryTrustLedger`] holds before it stops distinguishing
/// them.
///
/// One scope is a session id and a hash-set entry. The number is high enough that a process
/// reaches it only by holding tens of thousands of *distinct* tainted conversations, and
/// bounded because an unbounded set reachable from a tool call is a way to spend a host's
/// memory.
pub const DEFAULT_CAPACITY: usize = 65_536;

/// A [`TrustLedger`] that remembers tainted scopes in this process, and forgets them when it
/// exits.
///
/// This is the registry's default, so provenance is tracked in every deployment rather than
/// only in one that opted in. It has one property a deployment must know about, and it is a
/// gap rather than a subtlety: **taint does not survive a restart.** The honest unit of trust
/// is a transcript, transcripts are durable, and this ledger is not — so a session resumed
/// after a restart is resumed with whatever it was told still in its window and a ledger that
/// no longer knows. A durable implementation of [`TrustLedger`] closes that; the contract is
/// in `aik-api` precisely so one can be substituted without anything above noticing.
///
/// # Saturation
///
/// At [`DEFAULT_CAPACITY`] distinct tainted scopes, the ledger stops being able to say which
/// conversations are tainted, so it says all of them are. It does not evict, because evicting
/// a tainted scope is indistinguishable from declaring it clean, and it does not fail,
/// because failing to record a taint would mean an untrusted page had been read into a
/// conversation with nothing to show for it. Saturation is one-way for the life of the
/// process, and the deployment that reaches it wants the durable ledger, not a bigger number.
#[derive(Debug)]
pub struct InMemoryTrustLedger {
    state: Mutex<State>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct State {
    tainted: HashSet<TrustScope>,
    saturated: bool,
}

impl Default for InMemoryTrustLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTrustLedger {
    /// Creates an empty ledger holding up to [`DEFAULT_CAPACITY`] tainted scopes.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates an empty ledger with a specific capacity.
    ///
    /// A capacity of zero saturates on the first taint, which is a legitimate — if blunt —
    /// way to say "treat every conversation in this process as tainted once anything is".
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            capacity,
        }
    }

    /// Whether the ledger has stopped distinguishing scopes. See [Saturation](Self#saturation).
    pub fn is_saturated(&self) -> bool {
        self.lock().saturated
    }

    /// How many scopes are known to be tainted.
    pub fn tainted_count(&self) -> usize {
        self.lock().tainted.len()
    }

    /// A poisoned mutex is recovered from rather than propagated.
    ///
    /// The only code holding this lock is the two methods below, neither of which can panic
    /// while holding it, so a poisoned lock means something else in the process is already
    /// unwinding. The contents are still exactly what the last writer left, and the
    /// fail-closed reading is to keep using them: dropping the set would forget taints, and
    /// erroring would stop tools working because an unrelated task panicked.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl TrustLedger for InMemoryTrustLedger {
    async fn observe(&self, scope: &TrustScope, trust: Trust) -> Result<()> {
        if !trust.is_untrusted() {
            return Ok(());
        }
        let mut state = self.lock();
        if state.saturated || state.tainted.contains(scope) {
            return Ok(());
        }
        if state.tainted.len() >= self.capacity {
            state.saturated = true;
            state.tainted.clear();
            return Ok(());
        }
        state.tainted.insert(scope.clone());
        Ok(())
    }

    async fn trust_of(&self, scope: &TrustScope) -> Result<Trust> {
        let state = self.lock();
        if state.saturated || state.tainted.contains(scope) {
            Ok(Trust::Untrusted)
        } else {
            Ok(Trust::Trusted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(name: &str) -> TrustScope {
        TrustScope::new(name)
    }

    #[tokio::test]
    async fn an_unknown_scope_is_trusted() {
        let ledger = InMemoryTrustLedger::new();
        assert_eq!(ledger.trust_of(&scope("a")).await.unwrap(), Trust::Trusted);
    }

    #[tokio::test]
    async fn taint_is_remembered_and_does_not_leak_between_scopes() {
        let ledger = InMemoryTrustLedger::new();
        ledger.observe(&scope("a"), Trust::Untrusted).await.unwrap();

        assert_eq!(
            ledger.trust_of(&scope("a")).await.unwrap(),
            Trust::Untrusted
        );
        assert_eq!(ledger.trust_of(&scope("b")).await.unwrap(), Trust::Trusted);
    }

    #[tokio::test]
    async fn observing_trusted_content_never_clears_a_taint() {
        let ledger = InMemoryTrustLedger::new();
        ledger.observe(&scope("a"), Trust::Untrusted).await.unwrap();
        ledger.observe(&scope("a"), Trust::Trusted).await.unwrap();

        assert_eq!(
            ledger.trust_of(&scope("a")).await.unwrap(),
            Trust::Untrusted
        );
    }

    #[tokio::test]
    async fn saturation_taints_every_scope_rather_than_forgetting_one() {
        let ledger = InMemoryTrustLedger::with_capacity(2);
        for name in ["a", "b"] {
            ledger
                .observe(&scope(name), Trust::Untrusted)
                .await
                .unwrap();
        }
        assert!(!ledger.is_saturated());

        ledger.observe(&scope("c"), Trust::Untrusted).await.unwrap();

        assert!(ledger.is_saturated());
        // Including one that was never observed at all: the ledger can no longer tell, so it
        // reports the answer that refuses rather than the one that permits.
        assert_eq!(
            ledger.trust_of(&scope("never-seen")).await.unwrap(),
            Trust::Untrusted
        );
    }

    #[tokio::test]
    async fn trusted_observations_alone_never_saturate() {
        let ledger = InMemoryTrustLedger::with_capacity(0);
        ledger.observe(&scope("a"), Trust::Trusted).await.unwrap();

        assert!(!ledger.is_saturated());
        assert_eq!(ledger.trust_of(&scope("a")).await.unwrap(), Trust::Trusted);
    }
}
