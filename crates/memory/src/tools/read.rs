//! The two memory tools that only read: [`MemoryGetTool`] and [`MemoryQueryTool`].

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryKind, MemoryQuery};
use aik_api::permission::{ActionId, ResourceAuthorizer};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::write::IdInput;
use super::{
    ANY_KIND_RESOURCE, MemoryToolBinding, ensure_live, kind_resource, parse_kind, record_resource,
    record_schema, render_record,
};

/// The name [`MemoryGetTool`] registers under when none is given explicitly.
pub const DEFAULT_GET_NAME: &str = "memory.get";

/// The permission [`MemoryGetTool`] requires when none is given explicitly.
pub const DEFAULT_GET_PERMISSION: &str = "memory.get";

/// The name [`MemoryQueryTool`] registers under when none is given explicitly.
pub const DEFAULT_QUERY_NAME: &str = "memory.query";

/// The permission [`MemoryQueryTool`] requires when none is given explicitly.
pub const DEFAULT_QUERY_PERMISSION: &str = "memory.query";

/// How many records one [`MemoryQueryTool`] call returns when it does not say.
pub const DEFAULT_RESULTS: usize = 10;

/// The most records one [`MemoryQueryTool`] call will ever return, whatever it asks for.
///
/// Recall feeds straight back into a model's context, which is the scarcest resource in the
/// system and is separately budgeted by the context window. A cap here means one over-broad
/// query cannot spend that budget before the budget is even consulted.
pub const DEFAULT_MAX_RESULTS: usize = 50;

/// Arguments accepted by [`MemoryQueryTool`].
///
/// `deny_unknown_fields` also does the work of refusing the parts of
/// [`MemoryQuery`] no store implements yet: a model that asks for `text`, `embedding` or
/// `min_score` is told those are not arguments of this tool, rather than having the request
/// travel to the store to come back as
/// [`Error::Unsupported`](aik_core::Error::Unsupported).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryInput {
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    metadata: Map<String, Value>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Fetches one memory by id.
///
/// Returns `found: false` for an id nobody stored, and refuses — rather than reporting
/// absence — an id belonging to somebody else, which is the distinction
/// [`MemoryStore::get`](aik_api::memory::MemoryStore::get) draws and this tool passes
/// through unchanged. Record ids are UUIDs, so the refusal is not an oracle anything can
/// enumerate.
pub struct MemoryGetTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<MemoryToolBinding>,
}

impl std::fmt::Debug for MemoryGetTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryGetTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .finish()
    }
}

impl MemoryGetTool {
    pub(crate) fn new(binding: Arc<MemoryToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_GET_NAME),
            action: ActionId::new(DEFAULT_GET_PERMISSION),
            binding,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_GET_PERMISSION`].
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
impl Tool for MemoryGetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Fetches one of your stored memories by id, as returned when it was \
                          stored or found. Reports `found: false` if there is no such memory."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Id of the memory to fetch."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "found": { "type": "boolean" },
                    "record": record_schema()
                },
                "required": ["found"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: true,
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

        Ok(match store.get(&input.id, cx).await? {
            Some(record) => ToolOutcome::ok(json!({
                "found": true,
                "record": render_record(&record)
            })),
            None => ToolOutcome::ok(json!({ "found": false })),
        })
    }
}

/// Searches the memories the caller may act for, by kind and by metadata.
///
/// # What a query can and cannot see
///
/// [`MemoryStore::query`](aik_api::memory::MemoryStore::query) filters to the records the
/// calling principal may act for, and simply omits everybody else's rather than refusing —
/// an enumeration that errored on encountering a record it was not asking for would report
/// that the record exists. So this tool has no owner argument for the same reason
/// [`MemoryPutTool`](super::MemoryPutTool) has none: there is nothing to select, and the
/// scope is not the caller's to choose.
///
/// Matching is exact: the named kinds, and metadata keys that are present and equal.
/// Ranking is most recent first. Neither store ranks by meaning — see the crate
/// documentation — so this tool does not offer a search string that would only be ignored.
pub struct MemoryQueryTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<MemoryToolBinding>,
    default_results: usize,
    max_results: usize,
}

impl std::fmt::Debug for MemoryQueryTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryQueryTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .field("default_results", &self.default_results)
            .field("max_results", &self.max_results)
            .finish()
    }
}

impl MemoryQueryTool {
    pub(crate) fn new(binding: Arc<MemoryToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_QUERY_NAME),
            action: ActionId::new(DEFAULT_QUERY_PERMISSION),
            binding,
            default_results: DEFAULT_RESULTS,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_QUERY_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides the hard cap on how many records one call returns.
    ///
    /// The per-call default is lowered with it when it would otherwise exceed the cap, so
    /// the two can never be configured into contradicting each other.
    #[must_use]
    pub fn with_max_results(mut self, max_results: usize) -> Self {
        self.max_results = max_results;
        self.default_results = self.default_results.min(max_results);
        self
    }

    /// Overrides how many records a call that does not say returns.
    #[must_use]
    pub fn with_default_results(mut self, default_results: usize) -> Self {
        self.default_results = default_results.min(self.max_results);
        self
    }

    fn parse(&self, arguments: Value) -> Result<QueryInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }

    /// The kinds asked for, validated and deduplicated.
    fn kinds(input: &QueryInput) -> Result<Vec<MemoryKind>> {
        let mut kinds = input
            .kinds
            .iter()
            .map(|kind| parse_kind(kind))
            .collect::<Result<Vec<_>>>()?;
        kinds.sort();
        kinds.dedup();
        Ok(kinds)
    }

    /// How many records to return: what was asked for, never more than the cap.
    fn limit(&self, requested: Option<usize>) -> Result<usize> {
        match requested {
            Some(0) => Err(Error::InvalidArgument(
                "`limit` must be at least 1".to_owned(),
            )),
            Some(limit) => Ok(limit.min(self.max_results)),
            None => Ok(self.default_results.min(self.max_results)),
        }
    }
}

#[async_trait]
impl Tool for MemoryQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Searches your stored memories, most recent first. Filter by `kinds` \
                          and by exact `metadata` values; omit both to see the most recent of \
                          everything you remember. Only your own memories are searched. \
                          Matching is exact, not semantic."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kinds": {
                        "type": "array",
                        "items": { "type": "string", "maxLength": super::MAX_KIND_LENGTH },
                        "description": "Kinds to restrict the search to. Omit for all kinds."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Metadata keys that must be present and exactly equal."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": self.max_results,
                        "description": "Maximum records to return."
                    }
                },
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "limit": { "type": "integer" },
                    "records": { "type": "array", "items": record_schema() }
                },
                "required": ["count", "limit", "records"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: true,
        }
    }

    /// Declares one resource per kind asked for, or [`ANY_KIND_RESOURCE`] for a query that
    /// names none.
    ///
    /// Every claim is authorized before the tool runs, so a policy allowing recall of some
    /// kinds and not others refuses a call that asks for both together rather than silently
    /// returning the half it allows. That is the intended reading: the model asked one
    /// question, and the answer to it is no.
    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let kinds = Self::kinds(&input)?;
        if kinds.is_empty() {
            return Ok(vec![ResourceClaim::new(
                self.action.clone(),
                ANY_KIND_RESOURCE,
            )]);
        }
        Ok(kinds
            .iter()
            .map(|kind| ResourceClaim::new(self.action.clone(), kind_resource(kind)))
            .collect())
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;
        let kinds = Self::kinds(&input)?;
        let limit = self.limit(input.limit)?;
        let store = self.binding.store()?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        let query = MemoryQuery {
            kinds,
            metadata: input.metadata,
            limit: Some(limit),
            // Left unset deliberately: these tools expose no semantic search, so there is
            // nothing a caller could have put here. See `QueryInput`.
            ..MemoryQuery::default()
        };
        let matches = store.query(&query, cx).await?;
        let records: Vec<Value> = matches
            .iter()
            .map(|matched| render_record(&matched.record))
            .collect();

        Ok(ToolOutcome::ok(json!({
            "count": records.len(),
            "limit": limit,
            "records": records
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_tool() -> MemoryQueryTool {
        MemoryQueryTool::new(Arc::new(MemoryToolBinding::new()))
    }

    #[test]
    fn a_limit_is_capped_and_defaulted() {
        let tool = query_tool();
        assert_eq!(tool.limit(None).expect("a default"), DEFAULT_RESULTS);
        assert_eq!(tool.limit(Some(3)).expect("as asked"), 3);
        assert_eq!(
            tool.limit(Some(usize::MAX)).expect("capped"),
            DEFAULT_MAX_RESULTS
        );
        assert!(tool.limit(Some(0)).is_err());
    }

    #[test]
    fn lowering_the_cap_lowers_the_default_with_it() {
        let tool = query_tool().with_max_results(2);
        assert_eq!(tool.limit(None).expect("a default"), 2);
        assert_eq!(tool.limit(Some(10)).expect("capped"), 2);
    }

    #[test]
    fn an_unkinded_query_claims_the_any_kind_resource() {
        let tool = query_tool();
        let claims = tool.planned_resources(&json!({})).expect("valid arguments");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].resource.as_str(), ANY_KIND_RESOURCE);
    }

    #[test]
    fn a_kinded_query_claims_one_resource_per_distinct_kind() {
        let tool = query_tool();
        let claims = tool
            .planned_resources(&json!({ "kinds": ["fact", "note", "fact"] }))
            .expect("valid arguments");
        let resources: Vec<&str> = claims.iter().map(|c| c.resource.as_str()).collect();
        assert_eq!(resources, vec!["kind/fact", "kind/note"]);
    }

    #[test]
    fn semantic_and_owner_arguments_are_refused_outright() {
        let tool = query_tool();
        for arguments in [
            json!({ "text": "what does alice like?" }),
            json!({ "embedding": [0.1, 0.2] }),
            json!({ "min_score": 0.5 }),
            json!({ "owner": "alice" }),
            json!({ "principal": "alice" }),
        ] {
            assert!(
                tool.parse(arguments.clone()).is_err(),
                "{arguments} should not parse"
            );
        }
    }
}
