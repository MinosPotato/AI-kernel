//! Who a record belongs to, and who may touch it.
//!
//! The rule itself is [`Principal::may_act_for`], which lives in `aik-api` because the
//! context store asks the same question of a session and two copies of a security rule are
//! two things to keep in step. What lives here is only the part that is specific to a memory
//! record: which principal an [`ExecutionContext`] counts as, and what the refusal says.
//!
//! # Why a record is owned at all
//!
//! It was not, until the store had more than one caller. A single interactive user sharing a
//! process with nothing else needs no boundary; a scheduler running jobs as different
//! principals against the same file does, and retrofitting one onto stored data is far more
//! expensive than storing it from the start. So the owner is recorded now, on the same terms
//! the transcript store already uses: assigned from the context, never from the payload.
//!
//! The limit is the same one stated there, too. In-process code can construct an
//! `ExecutionContext` naming any principal, so this is a boundary against a model — which
//! can never construct one — and defence in depth against a confused caller. It is not a
//! boundary against hostile code already inside the process.

use aik_api::memory::MemoryId;
use aik_api::permission::{Principal, PrincipalId};
use aik_core::{Error, Result};

/// Fails closed unless `principal` may act for the record's `owner`.
///
/// Used by the methods that name one record. [`MemoryStore::query`](aik_api::memory::MemoryStore::query)
/// deliberately does not call this: it filters instead, because an enumeration that errored
/// on encountering someone else's record would report that the record exists.
pub(crate) fn authorize(id: &MemoryId, owner: &PrincipalId, principal: &Principal) -> Result<()> {
    if principal.may_act_for(owner) {
        return Ok(());
    }
    Err(Error::PermissionDenied(format!(
        "memory record `{id}` belongs to `{owner}`, not to `{}`",
        principal.id
    )))
}
