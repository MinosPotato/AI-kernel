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

/// The longest search text [`MemoryQueryTool`] advertises.
///
/// A search string is embedded, which costs a model call proportional to its length; a
/// bound here keeps one over-long query from turning recall into a second inference. It is
/// generous compared to any real question and small compared to a record.
pub const MAX_QUERY_TEXT_LENGTH: usize = 1_024;

/// The most records one [`MemoryQueryTool`] call will ever return, whatever it asks for.
///
/// Recall feeds straight back into a model's context, which is the scarcest resource in the
/// system and is separately budgeted by the context window. A cap here means one over-broad
/// query cannot spend that budget before the budget is even consulted.
pub const DEFAULT_MAX_RESULTS: usize = 50;

/// Arguments accepted by [`MemoryQueryTool`].
///
/// `deny_unknown_fields` refuses everything this tool deliberately does not offer — an
/// `owner`, and [`MemoryQuery::embedding`], which is a vector no model has and no model
/// should be asked to produce as JSON. `text` and `min_score` parse here whatever the store
/// can do, and are refused with a reason when it cannot; the schema is what stops a model
/// from asking in the first place.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryInput {
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    metadata: Map<String, Value>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    min_score: Option<f32>,
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
/// Kinds and metadata always match exactly. What `text` adds, where the bound store has an
/// embedding model, is *ranking*: the exact filters still decide which records are eligible,
/// and similarity decides the order among them and which of them clear `min_score`.
///
/// Whether `text` is offered at all is decided by the store, not by this tool: a tool bound
/// to a store that reports no
/// [`MemoryCapabilities::semantic_text`](aik_api::memory::MemoryCapabilities::semantic_text)
/// leaves both arguments out of its schema and refuses them if asked anyway, so a model is
/// never shown a search it would only be told it cannot have.
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

    /// Whether the bound store can rank by meaning.
    ///
    /// An unbound tool answers `false`: [`Tool::spec`] is called while the registry is being
    /// built, before this tool's component has bound anything, and a schema is not the place
    /// to raise a wiring error. Advertising the smaller set until the store is known is the
    /// conservative direction — it withholds a capability rather than promising one — and by
    /// the time a model sees a catalogue, the binding is long since filled.
    fn semantic(&self) -> bool {
        self.binding
            .store()
            .map(|store| store.capabilities().semantic_text)
            .unwrap_or(false)
    }

    /// Validates the semantic arguments, then refuses them if the bound store cannot honour
    /// them.
    ///
    /// Shape first, capability second, deliberately: an argument that is malformed is
    /// malformed whatever the store can do, and checking capability first would report a
    /// missing embedding model to a caller whose real mistake was a `min_score` of 4.
    fn check_semantic(&self, input: &QueryInput) -> Result<()> {
        if let Some(text) = &input.text {
            if text.trim().is_empty() {
                return Err(Error::InvalidArgument(
                    "`text` is empty; omit it to list by kind and metadata instead".to_owned(),
                ));
            }
            // Enforced here and not only advertised in the schema: a schema is what a model
            // is told, and the arguments are whatever actually arrived. Without this, one
            // call could hand the embedding model an arbitrarily long input.
            if text.len() > MAX_QUERY_TEXT_LENGTH {
                return Err(Error::InvalidArgument(format!(
                    "`text` must be at most {MAX_QUERY_TEXT_LENGTH} bytes, got {}",
                    text.len()
                )));
            }
        }
        if let Some(minimum) = input.min_score {
            if input.text.is_none() {
                return Err(Error::InvalidArgument(
                    "`min_score` filters similarity scores, so it needs a `text` to score \
                     against"
                        .to_owned(),
                ));
            }
            if !(-1.0..=1.0).contains(&minimum) {
                return Err(Error::InvalidArgument(format!(
                    "`min_score` is a cosine similarity and must be between -1 and 1; got \
                     {minimum}"
                )));
            }
        }
        if (input.text.is_some() || input.min_score.is_some()) && !self.semantic() {
            return Err(Error::InvalidArgument(format!(
                "`{}` has no `text` or `min_score` argument: the memory store it is bound to \
                 has no embedding model, so it cannot search by meaning",
                self.name
            )));
        }
        Ok(())
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
        let semantic = self.semantic();
        let description = if semantic {
            "Searches your stored memories. Give `text` to find the memories that mean the \
             most similar thing, best match first; omit it to list the most recent instead. \
             Filter either kind of search by `kinds` and by exact `metadata` values. Only \
             your own memories are searched."
        } else {
            "Searches your stored memories, most recent first. Filter by `kinds` and by \
             exact `metadata` values; omit both to see the most recent of everything you \
             remember. Only your own memories are searched. Matching is exact, not semantic."
        };

        let mut properties = json!({
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
        });
        if semantic {
            let properties = properties.as_object_mut().expect("an object literal");
            properties.insert(
                "text".to_owned(),
                json!({
                    "type": "string",
                    "maxLength": MAX_QUERY_TEXT_LENGTH,
                    "description": "What to search for. Results are ordered by how close \
                                    their meaning is to this."
                }),
            );
            properties.insert(
                "min_score".to_owned(),
                json!({
                    "type": "number",
                    "minimum": -1,
                    "maximum": 1,
                    "description": "Drop results less similar than this. Needs `text`. \
                                    1 is identical, 0 is unrelated."
                }),
            );
        }

        ToolSpec {
            name: self.name.clone(),
            description: description.to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "limit": { "type": "integer" },
                    "records": { "type": "array", "items": record_schema() },
                    "scores": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "Similarity of each record, in the same order. \
                                        Present only for a `text` search."
                    }
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
        self.check_semantic(&input)?;
        let kinds = Self::kinds(&input)?;
        let limit = self.limit(input.limit)?;
        let store = self.binding.store()?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        let query = MemoryQuery {
            kinds,
            metadata: input.metadata,
            limit: Some(limit),
            text: input.text,
            min_score: input.min_score,
            // Never set from an argument: `embedding` is a raw vector, which is not something
            // a model has, and accepting one would let a caller search a space the store
            // never agreed to. See `QueryInput`.
            embedding: None,
        };
        let matches = store.query(&query, cx).await?;
        let records: Vec<Value> = matches
            .iter()
            .map(|matched| render_record(&matched.record))
            .collect();

        let mut result = json!({
            "count": records.len(),
            "limit": limit,
            "records": records
        });
        // Scores travel beside the records rather than inside them, because a score belongs
        // to *this* search and not to the memory: rendering it into the record would make
        // the same memory look different depending on how it was found.
        if let Some(scores) = matches
            .iter()
            .map(|matched| matched.score)
            .collect::<Option<Vec<f32>>>()
        {
            result
                .as_object_mut()
                .expect("an object literal")
                .insert("scores".to_owned(), json!(scores));
        }

        Ok(ToolOutcome::ok(result))
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
    fn a_raw_embedding_and_an_owner_never_parse() {
        let tool = query_tool();
        for arguments in [
            json!({ "embedding": [0.1, 0.2] }),
            json!({ "owner": "alice" }),
            json!({ "principal": "alice" }),
        ] {
            assert!(
                tool.parse(arguments.clone()).is_err(),
                "{arguments} should not parse"
            );
        }
    }

    #[test]
    fn an_unbound_tool_advertises_and_accepts_no_semantic_search() {
        let tool = query_tool();
        let schema = tool.spec().input_schema;
        let properties = schema["properties"].as_object().expect("an object");
        assert!(!properties.contains_key("text"), "{schema}");
        assert!(!properties.contains_key("min_score"), "{schema}");

        let input = tool
            .parse(json!({ "text": "what does alice like?" }))
            .expect("it parses, and is refused with a reason");
        let error = tool.check_semantic(&input).expect_err("no embedding model");
        assert_eq!(error.kind(), aik_core::ErrorKind::InvalidArgument);
    }

    /// Each case names the phrase its *own* branch produces, so none of them can pass by
    /// tripping the "no embedding model" refusal that also applies to this unbound tool.
    #[test]
    fn a_malformed_semantic_argument_is_refused_on_its_own_terms() {
        let tool = query_tool();
        for (arguments, expected) in [
            (
                json!({ "text": "tea", "min_score": 1.5 }),
                "between -1 and 1",
            ),
            (
                json!({ "text": "tea", "min_score": -2.0 }),
                "between -1 and 1",
            ),
            (json!({ "min_score": 0.5 }), "needs a `text`"),
            (json!({ "text": "   " }), "is empty"),
            (
                json!({ "text": "x".repeat(MAX_QUERY_TEXT_LENGTH + 1) }),
                "at most",
            ),
        ] {
            let input = tool.parse(arguments.clone()).expect("it parses");
            let error = tool
                .check_semantic(&input)
                .expect_err("{arguments} should be refused");
            assert!(
                format!("{error}").contains(expected),
                "{arguments}: expected `{expected}`, got `{error}`"
            );
        }
    }
}
