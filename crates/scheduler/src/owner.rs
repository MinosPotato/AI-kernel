//! Who a job belongs to, who may touch it, and whose authority its firings carry.
//!
//! The rule for "may act for" is [`Principal::may_act_for`], which lives in `aik-api` because
//! the memory and context stores ask exactly the same question of a record and of a session.
//! What lives here is the part specific to a scheduled job: which principal an
//! [`ExecutionContext`] counts as, what the refusal says, and — the part no other subsystem
//! needs — what principal a firing runs as once the caller who scheduled it is long gone.

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_api::scheduler::JobId;
use aik_core::{Error, Result};

/// The identifier every firing runs under.
///
/// A firing is the system acting, so it is a [`PrincipalKind::System`] identity — but a
/// *named* one rather than the generic [`Principal::system`], so that a policy rule can speak
/// about scheduled work specifically ("`scheduler` may not write outside the workspace")
/// without also constraining startup work and everything else the system does on its own
/// behalf.
pub const RUN_PRINCIPAL: &str = "scheduler";

/// The principal a context is acting as.
///
/// A context with no principal is the system acting for itself — its own identity, not a
/// wildcard — exactly as it is in the memory store and in
/// [`ToolRegistry`](aik_api::tool::ToolRegistry).
pub(crate) fn principal_of(cx: &ExecutionContext) -> Principal {
    cx.principal.clone().unwrap_or_else(Principal::system)
}

/// Fails closed unless `principal` may act for the job's `owner`.
///
/// Used by [`Scheduler::schedule`](aik_api::scheduler::Scheduler::schedule) when it is
/// replacing a job and by [`Scheduler::cancel`](aik_api::scheduler::Scheduler::cancel), both
/// of which name one job.
/// [`Scheduler::list`](aik_api::scheduler::Scheduler::list) deliberately does not call this:
/// it filters instead, because an enumeration that errored on encountering someone else's job
/// would report that the job exists.
pub(crate) fn authorize(id: &JobId, owner: &PrincipalId, principal: &Principal) -> Result<()> {
    if principal.may_act_for(owner) {
        return Ok(());
    }
    Err(Error::PermissionDenied(format!(
        "job `{id}` belongs to `{owner}`, not to `{}`",
        principal.id
    )))
}

/// The principal one firing of a job owned by `owner` runs as.
///
/// [`RUN_PRINCIPAL`] acting on behalf of the owner: the system, doing something for someone,
/// which is precisely what a scheduled job is and precisely what
/// [`on_behalf_of`](Principal::on_behalf_of) exists to express. Three consequences, all
/// intended:
///
/// * A firing can reach the owner's own resources — their memories, their sessions — because
///   [`Principal::may_act_for`] accepts a delegate.
/// * A policy engine can tell "alice asked for this" from "a job is doing this for alice",
///   which matters most for exactly the actions worth gating: an unattended firing at 3am is
///   not the same event as a person typing a command.
/// * Delegation does not compound. A job scheduled by an agent that was itself acting for a
///   user is owned by the *agent*, so its firings act for the agent and not for the user; the
///   second hop is dropped rather than replayed. That is a narrowing, and narrowing is the
///   direction an authority derived from a stored record should fail in.
pub(crate) fn run_principal(owner: &PrincipalId) -> Principal {
    Principal::new(RUN_PRINCIPAL, PrincipalKind::System).on_behalf_of(owner.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_without_a_principal_is_the_system() {
        assert_eq!(principal_of(&ExecutionContext::new()), Principal::system());
    }

    #[test]
    fn the_owner_and_their_delegates_are_authorized_and_nobody_else_is() {
        let id = JobId::new("nightly");
        let owner = PrincipalId::new("alice");

        let alice = Principal::new("alice", PrincipalKind::User);
        let her_agent = Principal::new("agent", PrincipalKind::Agent).on_behalf_of("alice");
        let mallory = Principal::new("mallory", PrincipalKind::User);

        assert!(authorize(&id, &owner, &alice).is_ok());
        assert!(authorize(&id, &owner, &her_agent).is_ok());

        let error = authorize(&id, &owner, &mallory).unwrap_err();
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
        assert!(error.to_string().contains("belongs to `alice`"), "{error}");
    }

    #[test]
    fn the_system_principal_is_not_a_master_key() {
        let error = authorize(
            &JobId::new("nightly"),
            &PrincipalId::new("alice"),
            &Principal::system(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
    }

    #[test]
    fn a_firing_acts_for_its_owner_without_becoming_them() {
        let run = run_principal(&PrincipalId::new("alice"));

        assert_eq!(run.kind, PrincipalKind::System);
        assert_eq!(run.id, PrincipalId::new(RUN_PRINCIPAL));
        assert!(run.may_act_for(&PrincipalId::new("alice")));
        // It is not Alice, so anything keyed to the *identity* rather than to delegation
        // still tells the two apart.
        assert_ne!(run.id, PrincipalId::new("alice"));
    }

    #[test]
    fn delegation_does_not_compound_across_scheduling() {
        // An agent working for Alice schedules a job. The job is the agent's, so its firings
        // act for the agent -- and not, transitively, for Alice.
        let agent = Principal::new("agent", PrincipalKind::Agent).on_behalf_of("alice");
        let run = run_principal(&agent.id);

        assert!(run.may_act_for(&PrincipalId::new("agent")));
        assert!(
            !run.may_act_for(&PrincipalId::new("alice")),
            "a stored job must not replay a delegation chain it did not record"
        );
    }
}
