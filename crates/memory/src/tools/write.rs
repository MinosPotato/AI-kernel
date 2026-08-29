//! The two memory tools that change something: [`MemoryPutTool`] and [`MemoryDeleteTool`].

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryId, MemoryRecord};
use aik_api::permission::{ActionId, ResourceAuthorizer};
use aik_api::provenance::{Reach, Trust};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{MemoryToolBinding, ensure_live, kind_resource, parse_kind, record_resource};

/// The name [`MemoryPutTool`] registers under when none is given explicitly.
pub const DEFAULT_PUT_NAME: &str = "memory.put";

/// The permission [`MemoryPutTool`] requires when none is given explicitly.
pub const DEFAULT_PUT_PERMISSION: &str = "memory.put";

/// The name [`MemoryDeleteTool`] registers under when none is given explicitly.
pub const DEFAULT_DELETE_NAME: &str = "memory.delete";

/// The permission [`MemoryDeleteTool`] requires when none is given explicitly.
pub const DEFAULT_DELETE_PERMISSION: &str = "memory.delete";

/// The largest record one [`MemoryPutTool`] call will store, in bytes of serialised content
/// plus metadata.
///
/// Generous for a fact, a preference or a summary — the things a memory is for — and far too
/// small to page a transcript, a file or a model's whole output into a durable store one
/// call at a time. A model that needs more is being told the wrong thing to remember.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 16 * 1024;

/// The longest lifetime [`MemoryPutTool`] accepts, in seconds: a hundred years.
///
/// The cap exists because expiry is computed as an offset from now, and an unbounded offset
/// overflows the millisecond timestamp it is added to. A hundred years is indistinguishable
/// from "never" — which is what omitting `ttl_seconds` already means — so nothing expressible
/// is lost.
pub const MAX_TTL_SECONDS: u64 = 100 * 365 * 24 * 60 * 60;

/// Arguments accepted by [`MemoryPutTool`].
///
/// `deny_unknown_fields` is load-bearing rather than tidy: it is what turns a model's
/// invented `"owner"` into a refused call instead of a field somebody might one day read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PutInput {
    kind: String,
    content: Value,
    #[serde(default)]
    metadata: Map<String, Value>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    id: Option<MemoryId>,
}

/// Arguments accepted by [`MemoryGetTool`](super::MemoryGetTool) and [`MemoryDeleteTool`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdInput {
    pub(crate) id: MemoryId,
}

/// Stores a memory for whoever the call is running as.
///
/// # Ownership
///
/// The tool never names an owner and has no way to express one. The record it builds is
/// handed to [`MemoryStore::put`](aik_api::memory::MemoryStore::put), which stamps the owner
/// from the [`ExecutionContext`] — so a call running as Alice writes Alice's memory, a call
/// running as an agent acting for Alice writes the *agent's* memory, and a call that names
/// the id of somebody else's record is refused rather than overwriting it. None of that is
/// decided here; see [`Principal::may_act_for`](aik_api::permission::Principal::may_act_for),
/// which is where it is decided for every subsystem at once.
///
/// # What a model may and may not set
///
/// | Field | From |
/// |-------|------|
/// | `kind`, `content`, `metadata` | the model (validated and size-bounded) |
/// | `id` | the model, but only to replace a record it may already act for |
/// | `created_at` | the kernel clock |
/// | `expires_at` | the kernel clock plus the model's `ttl_seconds` |
/// | `owner` | the [`ExecutionContext`], never an argument |
///
/// Supplying an `id` replaces that record outright — the same upsert
/// [`MemoryStore::put`](aik_api::memory::MemoryStore::put) has always performed, including a
/// fresh `created_at`. It does not merge, and it cannot take a record away from its owner.
pub struct MemoryPutTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<MemoryToolBinding>,
    max_record_bytes: usize,
}

impl std::fmt::Debug for MemoryPutTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryPutTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .field("max_record_bytes", &self.max_record_bytes)
            .finish()
    }
}

impl MemoryPutTool {
    pub(crate) fn new(binding: Arc<MemoryToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_PUT_NAME),
            action: ActionId::new(DEFAULT_PUT_PERMISSION),
            binding,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_PUT_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides the maximum size of a single stored record.
    #[must_use]
    pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self {
        self.max_record_bytes = max_record_bytes;
        self
    }

    fn parse(&self, arguments: Value) -> Result<PutInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }

    /// How many bytes this record would occupy, counting only what the caller supplied.
    fn payload_bytes(input: &PutInput) -> Result<usize> {
        let content = serde_json::to_vec(&input.content).map_err(Error::Serialization)?;
        let metadata = serde_json::to_vec(&input.metadata).map_err(Error::Serialization)?;
        Ok(content.len() + metadata.len())
    }
}

#[async_trait]
impl Tool for MemoryPutTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Remembers something for later, outside the current conversation: a \
                          fact, a preference, a summary worth keeping. `kind` classifies the \
                          memory and is what a later search filters on. The memory belongs to \
                          whoever this call is running as; that is decided by the system and \
                          cannot be set here. Pass `id` to replace one of your own existing \
                          memories."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Classifier, e.g. `fact`, `preference`, `summary`.",
                        "maxLength": super::MAX_KIND_LENGTH
                    },
                    "content": {
                        "description": "The memory itself. Any JSON value."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Filterable annotations, e.g. subject or source. \
                                        Matched exactly by a later search."
                    },
                    "ttl_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TTL_SECONDS,
                        "description": "Forget this memory after that many seconds. Omit to \
                                        keep it indefinitely."
                    },
                    "id": {
                        "type": "string",
                        "description": "Id of an existing memory of yours to replace. Omit to \
                                        record a new one."
                    }
                },
                "required": ["kind", "content"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "kind": { "type": "string" },
                    "created_at": { "type": "integer" },
                    "expires_at": { "type": "integer" },
                    "error": { "type": "string" }
                },
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
            // The output is an id and a timestamp this tool wrote, not the memory itself.
            output_trust: Trust::Trusted,
            // What it changes outlives the conversation, which is the whole point of it: a
            // record written now is read back by a run tomorrow.
            reach: Reach::Mutating,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let kind = parse_kind(&input.kind)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            kind_resource(&kind),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        // The only resource this call touches is the kind, which `planned_resources`
        // declared and the registry authorized before this ran. There is nothing discovered
        // mid-call to ask the authorizer about.
        let mut input = self.parse(arguments)?;
        let kind = parse_kind(&input.kind)?;
        let store = self.binding.store()?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        // Stamped before the size is measured, so the record that is checked is the record
        // that is stored. See `stamp_trust` for why a model's own metadata cannot survive
        // under this key.
        super::stamp_trust(&mut input.metadata, cx);

        let bytes = Self::payload_bytes(&input)?;
        if bytes > self.max_record_bytes {
            return Ok(ToolOutcome::error(json!({
                "error": format!(
                    "memory is {bytes} bytes; the limit is {} — store a summary instead",
                    self.max_record_bytes
                )
            })));
        }
        if let Some(ttl) = input.ttl_seconds
            && (ttl == 0 || ttl > MAX_TTL_SECONDS)
        {
            return Err(Error::InvalidArgument(format!(
                "`ttl_seconds` must be between 1 and {MAX_TTL_SECONDS}"
            )));
        }

        let created_at = self.binding.now()?;
        let mut record = MemoryRecord::new(kind.clone(), input.content, created_at);
        // `MemoryRecord::owner` is deliberately left as `MemoryRecord::new` set it: the
        // store overwrites it from `cx`, and anything written here would be discarded. It is
        // not set to a principal read from the arguments because there is no such argument.
        record.metadata = input.metadata;
        record.expires_at = input
            .ttl_seconds
            .map(|ttl| created_at.saturating_add(std::time::Duration::from_secs(ttl)));
        if let Some(id) = input.id {
            record.id = id;
        }
        let id = record.id;
        let expires_at = record.expires_at;

        store.put(record, cx).await?;

        let mut output = Map::new();
        output.insert("id".to_owned(), json!(id.to_string()));
        output.insert("kind".to_owned(), json!(kind.as_str()));
        output.insert("created_at".to_owned(), json!(created_at.as_millis()));
        if let Some(expires_at) = expires_at {
            output.insert("expires_at".to_owned(), json!(expires_at.as_millis()));
        }
        Ok(ToolOutcome::ok(Value::Object(output)))
    }
}

/// Forgets one memory, by id.
///
/// Deleting is separated from storing so that a deployment can grant one without the other:
/// an agent that may record what it learns but may not erase what it recorded is a
/// meaningful configuration, and it is expressible here as registering [`MemoryPutTool`]
/// without this one, or as denying `memory.delete` in policy.
///
/// The store refuses another principal's record rather than deleting it, and reports a
/// record that was never there as `deleted: false` rather than an error — the same
/// distinction [`MemoryStore::delete`](aik_api::memory::MemoryStore::delete) draws.
pub struct MemoryDeleteTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<MemoryToolBinding>,
}

impl std::fmt::Debug for MemoryDeleteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDeleteTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .finish()
    }
}

impl MemoryDeleteTool {
    pub(crate) fn new(binding: Arc<MemoryToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_DELETE_NAME),
            action: ActionId::new(DEFAULT_DELETE_PERMISSION),
            binding,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_DELETE_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    fn parse(&self, arguments: Value) -> Result<IdInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Forgets one of your stored memories, by id. Reports whether there \
                          was one to forget. Memories belonging to anyone else cannot be \
                          deleted."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Id of the memory to forget."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "deleted": { "type": "boolean" }
                },
                "required": ["id", "deleted"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
            output_trust: Trust::Trusted,
            reach: Reach::Mutating,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            record_resource(&input.id),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;
        let store = self.binding.store()?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        let deleted = store.delete(&input.id, cx).await?;
        Ok(ToolOutcome::ok(json!({
            "id": input.id.to_string(),
            "deleted": deleted
        })))
    }
}
