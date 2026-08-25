//! What every test in this crate needs and nothing else.
//!
//! Compiled separately into each integration test binary, so anything one of them does not use
//! is dead code from that binary's point of view. The alternative is a support module split
//! along the same lines as the tests, which would be two files to keep in step for no gain.
#![allow(dead_code)]

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, Principal, PrincipalKind, ResourceAuthorizer, ResourceId};
use aik_core::Result;
use async_trait::async_trait;

/// A [`ResourceAuthorizer`] that must never be consulted.
///
/// [`ExecTool`](aik_exec::ExecTool) declares every resource it acts on up front, so the
/// mid-run authorizer the registry hands it has nothing to answer. A test that reached it
/// would mean a resource was touched that policy was never asked about, which is the failure
/// this panic exists to make loud rather than silent.
#[derive(Debug)]
pub(crate) struct Unasked;

#[async_trait]
impl ResourceAuthorizer for Unasked {
    async fn authorize(&self, action: &ActionId, resource: &ResourceId) -> Result<()> {
        panic!("nothing should be authorized mid-run: {action} on {resource}");
    }
}

/// An execution context attributed to an agent.
pub(crate) fn agent(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::Agent))
}
