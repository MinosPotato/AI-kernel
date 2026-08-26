//! Memory as something an agent can actually reach: four [`Tool`](aik_api::tool::Tool)s over
//! the existing [`MemoryStore`](aik_api::memory::MemoryStore).
//!
//! # Why tools, and why here
//!
//! An agent already has exactly one way to affect anything: it asks a model, the model asks
//! for a tool, and [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke) authorizes
//! and runs it. Everything the kernel needs memory to inherit — a policy decision before
//! anything happens, an audit event afterwards, one
//! [`ExecutionContext`](aik_api::execution::ExecutionContext) carrying one principal from
//! the request through to the store — is a property of that path. Reaching
//! memory any other way would mean rebuilding all of it, worse, somewhere else:
//!
//! * **A service the agent loop holds directly** (`AgentLoop::with_memory`) puts recall and
//!   storage *inside* the loop, where no policy engine is consulted and no audit event is
//!   published. The loop would become a decision point, which is the one thing its design
//!   says it must never be.
//! * **A memory-aware context store** would make every append a potential memory write,
//!   driven by heuristics about what is worth keeping — a judgement this crate deliberately
//!   does not make, and one that would run with no way for policy to say no.
//! * **Tools** need none of that. The registry is already the single gated door; a memory
//!   tool is just another thing behind it, and the ownership rules the store already
//!   enforces do the rest.
//!
//! They live in this crate, next to the store, because they are written against the
//! [`MemoryStore`](aik_api::memory::MemoryStore) *contract* — nothing here knows whether it
//! is talking to [`InMemoryMemoryStore`](crate::InMemoryMemoryStore) or
//! [`RedbMemoryStore`](crate::RedbMemoryStore), and both are wired the same way. Putting
//! them in `aik-tools` or `aik-agent` instead would have made one of those crates depend on
//! this one for no gain.
//!
//! # The four tools
//!
//! | Tool | Permission | Resource claimed | Mutates |
//! |------|------------|------------------|---------|
//! | [`MemoryPutTool`] | `memory.put` | `kind/<kind>` | yes |
//! | [`MemoryGetTool`] | `memory.get` | `record/<id>` | no |
//! | [`MemoryDeleteTool`] | `memory.delete` | `record/<id>` | yes |
//! | [`MemoryQueryTool`] | `memory.query` | `kind/<kind>` per named kind, or [`ANY_KIND_RESOURCE`] | no |
//!
//! Four tools rather than one `memory` tool with an `operation` argument, for the reason
//! [`aik_fs`](https://docs.rs/aik-fs) registers its three separately: a deployment that
//! wants an agent to *recall* what it was told without being able to *write* or *forget*
//! registers only [`MemoryGetTool`] and [`MemoryQueryTool`], and a policy that wants the
//! same guarantee a second, independent way denies `memory.put` and `memory.delete`. An
//! operation selected by an argument can be neither.
//!
//! # Security
//!
//! The whole point of routing memory through tools is that a model's output reaches the
//! store as *content* and never as *authority*. Concretely:
//!
//! * **The owner is never an argument.** No tool has an `owner`, `principal` or
//!   `on_behalf_of` field, in any operation, and every input struct is
//!   `deny_unknown_fields` with `"additionalProperties": false` in its schema, so a model
//!   that invents one gets [`Error::InvalidArgument`] rather than having it ignored. The
//!   owner is stamped by the store from the
//!   [`ExecutionContext`](aik_api::execution::ExecutionContext) — see the crate's own
//!   documentation for why it is stamped in one place —
//!   and these tools never construct a context of their own: they pass down the one the
//!   registry handed them, which is the one the agent loop derived from the request.
//! * **Delegation is inherited, not interpreted.** A run acting
//!   [`on_behalf_of`](aik_api::permission::Principal::on_behalf_of) somebody reaches the
//!   store as that principal, and
//!   [`may_act_for`](aik_api::permission::Principal::may_act_for) decides the rest. Nothing
//!   here reads `on_behalf_of`, widens it, or lets an argument affect it.
//! * **Timestamps come from the kernel clock.** `created_at` is taken from the clock the
//!   binding was given, and expiry is expressed as a *relative* `ttl_seconds`, so a model
//!   cannot forge when a memory was recorded or backdate one into a range someone else's
//!   query filters on.
//! * **Every write is bounded.** [`MemoryPutTool`] refuses a record whose content and
//!   metadata exceed [`DEFAULT_MAX_RECORD_BYTES`], and [`MemoryQueryTool`] caps how many
//!   records one call can pull back, so a model cannot fill a durable store or a context
//!   window from one call.
//! * **An unwired tool refuses.** A tool whose [`MemoryToolsComponent`] was never added to
//!   the kernel has no store, and says so instead of doing anything.
//!
//! What this does *not* defend against is in-process code that constructs an
//! `ExecutionContext` naming whoever it likes; that boundary is discussed in this crate's
//! own documentation and is unchanged here.
//!
//! # Wiring
//!
//! The store is published by [`MemoryComponent`](crate::MemoryComponent) during `init`, and
//! tools are handed to `ToolsComponent` before the kernel is built, so a tool cannot be
//! constructed around a store that does not exist yet. [`MemoryToolsComponent`] closes that
//! gap: the tools it hands out share one binding, which the component fills during its own
//! `init` — after the memory component's, which it declares as a dependency.
//!
//! ```no_run
//! use aik_core::prelude::*;
//! use aik_memory::{MemoryComponent, MemoryToolsComponent};
//! use aik_tools::ToolsComponent;
//!
//! # fn build() -> Result<Kernel> {
//! let memory_tools = MemoryToolsComponent::new();
//!
//! Kernel::builder()
//!     .component(MemoryComponent::new())
//!     .component(
//!         ToolsComponent::new()
//!             .with_tool(memory_tools.put())
//!             .with_tool(memory_tools.get())
//!             .with_tool(memory_tools.query())
//!             .with_tool(memory_tools.delete()),
//!     )
//!     .component(memory_tools)
//!     .build()
//! # }
//! ```

mod binding;
mod component;
mod read;
mod write;

use aik_api::memory::{MemoryKind, MemoryRecord};
use aik_api::permission::ResourceId;
use aik_core::clock::Clock;
use aik_core::{Error, Result};
use serde_json::{Map, Value, json};

pub(crate) use binding::MemoryToolBinding;
pub use component::{DEFAULT_TOOLS_COMPONENT_ID, MemoryToolsComponent};
pub use read::{
    DEFAULT_GET_NAME, DEFAULT_GET_PERMISSION, DEFAULT_MAX_RESULTS, DEFAULT_QUERY_NAME,
    DEFAULT_QUERY_PERMISSION, DEFAULT_RESULTS, MAX_QUERY_TEXT_LENGTH, MemoryGetTool,
    MemoryQueryTool,
};
pub use write::{
    DEFAULT_DELETE_NAME, DEFAULT_DELETE_PERMISSION, DEFAULT_MAX_RECORD_BYTES, DEFAULT_PUT_NAME,
    DEFAULT_PUT_PERMISSION, MAX_TTL_SECONDS, MemoryDeleteTool, MemoryPutTool,
};

/// Prefix of the [`ResourceId`] a memory *kind* is authorized under, e.g. `kind/preference`.
///
/// A policy rule can therefore scope an operation to a namespace of memories —
/// `{"action": "memory.put", "resource": "kind/note"}` — using the same prefix patterns it
/// uses for paths and action names.
pub const KIND_RESOURCE_PREFIX: &str = "kind/";

/// Prefix of the [`ResourceId`] one identified record is authorized under, e.g.
/// `record/0192...`.
///
/// Record ids are UUIDs assigned by the store, so a rule written against one is a rule about
/// exactly one memory; the useful patterns here are `record/*` and omitting the resource
/// entirely.
pub const RECORD_RESOURCE_PREFIX: &str = "record/";

/// The resource a [`MemoryQueryTool`] call that names no kind is authorized under.
///
/// A query with no kinds asks for *every* kind, so it cannot honestly be authorized as any
/// one of them. Claiming this instead means a policy that allows only `kind/note` refuses
/// the unrestricted query — the query has to say what it is looking for — while a policy
/// that allows `kind/*` or `*` allows it, since both already mean "any kind".
pub const ANY_KIND_RESOURCE: &str = "kind/*";

/// The longest [`MemoryKind`] these tools accept.
///
/// A kind is a short classifier that policy rules are written against, not a payload; the
/// content field is where a memory's substance belongs. Bounding it keeps a model from
/// smuggling arbitrary text into the resource string a policy engine and an audit log both
/// have to render.
pub const MAX_KIND_LENGTH: usize = 128;

/// Validates a model-supplied kind, or explains why it is not one.
///
/// Kinds reach a [`ResourceId`] and an audit event verbatim, so the checks are about what
/// can be *rendered and matched* unambiguously rather than about meaning: no empty string,
/// no surrounding whitespace to make two kinds look identical, no control characters, and a
/// bounded length. Anything else is allowed, `/` included — a hierarchical kind such as
/// `user/preference` is a reasonable thing to want, and prefix matching over it behaves
/// exactly as it does for a path.
pub(crate) fn parse_kind(raw: &str) -> Result<MemoryKind> {
    if raw.is_empty() {
        return Err(Error::InvalidArgument(
            "`kind` must not be empty".to_owned(),
        ));
    }
    if raw.trim() != raw {
        return Err(Error::InvalidArgument(
            "`kind` must not begin or end with whitespace".to_owned(),
        ));
    }
    if raw.len() > MAX_KIND_LENGTH {
        return Err(Error::InvalidArgument(format!(
            "`kind` must be at most {MAX_KIND_LENGTH} bytes, got {}",
            raw.len()
        )));
    }
    if raw.chars().any(char::is_control) {
        return Err(Error::InvalidArgument(
            "`kind` must not contain control characters".to_owned(),
        ));
    }
    Ok(MemoryKind::new(raw))
}

/// The resource a given kind is authorized under.
pub(crate) fn kind_resource(kind: &MemoryKind) -> ResourceId {
    ResourceId::new(format!("{KIND_RESOURCE_PREFIX}{kind}"))
}

/// The resource a given record id is authorized under.
pub(crate) fn record_resource(id: &aik_api::memory::MemoryId) -> ResourceId {
    ResourceId::new(format!("{RECORD_RESOURCE_PREFIX}{id}"))
}

/// What a model is shown of a stored record.
///
/// Deliberately not `serde_json::to_value(record)`. The stored struct carries an
/// [`owner`](MemoryRecord::owner) — decided by the kernel, not by whoever is reading — and
/// an embedding, and neither is anything a model needs in order to use a memory. Rendering
/// explicitly also means the shape a model sees is a decision rather than a consequence of
/// how the store happens to serialise.
pub(crate) fn render_record(record: &MemoryRecord) -> Value {
    let mut rendered = Map::new();
    rendered.insert("id".to_owned(), json!(record.id.to_string()));
    rendered.insert("kind".to_owned(), json!(record.kind.as_str()));
    rendered.insert("content".to_owned(), record.content.clone());
    rendered.insert(
        "metadata".to_owned(),
        Value::Object(record.metadata.clone()),
    );
    rendered.insert(
        "created_at".to_owned(),
        json!(record.created_at.as_millis()),
    );
    if let Some(expires_at) = record.expires_at {
        rendered.insert("expires_at".to_owned(), json!(expires_at.as_millis()));
    }
    Value::Object(rendered)
}

/// Refuses to start work whose context has already been cancelled or run out of time.
///
/// A store call is short, but it is not free, and a tool that ignored an expired context
/// would keep writing to a durable store after the run that asked for it was abandoned.
pub(crate) fn ensure_live(
    cx: &aik_api::execution::ExecutionContext,
    clock: &dyn Clock,
) -> Result<()> {
    if cx.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if cx.deadline.is_some_and(|deadline| clock.now() >= deadline) {
        return Err(Error::Timeout(std::time::Duration::ZERO));
    }
    Ok(())
}

/// The JSON schema fragment describing a rendered record, shared by the two tools that
/// return one so their declared outputs cannot drift apart.
pub(crate) fn record_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "kind": { "type": "string" },
            "content": {},
            "metadata": { "type": "object" },
            "created_at": { "type": "integer" },
            "expires_at": { "type": "integer" }
        },
        "required": ["id", "kind", "content", "metadata", "created_at"],
        "additionalProperties": false
    })
}

/// Direct-invocation helpers for the unit tests in this module tree.
///
/// A tool is normally only ever reached through
/// [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke), which is what authorizes
/// it. These tests call [`Tool::invoke`](aik_api::tool::Tool::invoke) directly because what
/// they are about is the tool's own behaviour — parsing, bounds, what it stores and what it
/// refuses. That the registry is the only door, and that policy is consulted at it, is
/// asserted where it belongs: against a real registry, in the cross-subsystem suite.
#[cfg(test)]
pub(crate) mod testing {
    use aik_api::execution::ExecutionContext;
    use aik_api::permission::{ActionId, Principal, PrincipalKind, ResourceAuthorizer, ResourceId};
    use aik_api::tool::{Tool, ToolOutcome};
    use aik_core::Result;
    use async_trait::async_trait;
    use serde_json::Value;

    /// Stands in for the authorizer a registry would supply. None of these tools ask it
    /// anything — every resource they touch is declared up front — so it only has to exist.
    #[derive(Debug)]
    pub(crate) struct NoDiscoveredResources;

    #[async_trait]
    impl ResourceAuthorizer for NoDiscoveredResources {
        async fn authorize(&self, _action: &ActionId, _resource: &ResourceId) -> Result<()> {
            unreachable!("these tools declare every resource they touch in advance")
        }
    }

    /// Invokes a tool the way a registry would, minus the authorization it would have done.
    pub(crate) async fn invoke(
        tool: &impl Tool,
        arguments: Value,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        tool.invoke(arguments, &NoDiscoveredResources, cx).await
    }

    /// A context acting as a named user.
    pub(crate) fn cx(principal: &str) -> ExecutionContext {
        ExecutionContext::new().with_principal(Principal::new(principal, PrincipalKind::User))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::memory::MemoryRecord;
    use aik_core::clock::Timestamp;

    #[test]
    fn a_kind_must_be_present_bounded_and_renderable() {
        assert!(parse_kind("preference").is_ok());
        assert!(parse_kind("user/preference").is_ok());
        assert!(parse_kind("").is_err());
        assert!(parse_kind(" fact").is_err());
        assert!(parse_kind("fact ").is_err());
        assert!(parse_kind("fa\nct").is_err());
        assert!(parse_kind(&"x".repeat(MAX_KIND_LENGTH)).is_ok());
        assert!(parse_kind(&"x".repeat(MAX_KIND_LENGTH + 1)).is_err());
    }

    #[test]
    fn resources_are_namespaced_so_a_kind_can_never_look_like_a_record() {
        let kind = parse_kind("record/whatever").expect("a valid kind");
        assert_eq!(kind_resource(&kind).as_str(), "kind/record/whatever");
        let id = aik_api::memory::MemoryId::new();
        assert_eq!(record_resource(&id).as_str(), format!("record/{id}"));
    }

    #[test]
    fn a_rendered_record_never_carries_its_owner_or_embedding() {
        let mut record = MemoryRecord::new("fact", json!({"n": 1}), Timestamp::from_millis(7));
        record.owner = aik_api::permission::PrincipalId::new("alice");
        record.embedding = Some(vec![0.5]);
        record.metadata.insert("source".to_owned(), json!("chat"));

        let rendered = render_record(&record);
        assert_eq!(rendered["kind"], json!("fact"));
        assert_eq!(rendered["content"], json!({"n": 1}));
        assert_eq!(rendered["metadata"]["source"], json!("chat"));
        assert_eq!(rendered["created_at"], json!(7));
        assert!(rendered.get("owner").is_none());
        assert!(rendered.get("embedding").is_none());
        assert!(rendered.get("expires_at").is_none());
    }
}
