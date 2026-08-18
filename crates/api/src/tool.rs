//! Tool contracts.
//!
//! A tool is a named, schema-described capability that a model or an agent can invoke.
//! Inputs and outputs are JSON described by JSON Schema, because that is what model
//! providers expect and what can cross a process or sandbox boundary unchanged.
//!
//! [`ToolSpec::required_permissions`] is the link to the [permission](crate::permission)
//! layer: a tool declares what it needs, and the runtime that invokes it asks the policy
//! engine before running it. The tool itself never enforces policy.

use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;
use crate::permission::ActionId;

aik_core::string_id! {
    /// Names a tool, e.g. `fs.read` or `hyprland.focus_window`.
    pub ToolName
}

/// What a tool is and how to call it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool's name.
    pub name: ToolName,
    /// What it does, written for a model to read.
    pub description: String,
    /// JSON Schema describing the input object.
    pub input_schema: Value,
    /// JSON Schema describing the output, when it is worth declaring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Permissions the runtime must obtain before invoking this tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_permissions: Vec<ActionId>,
    /// Whether the tool changes state.
    ///
    /// Read-only tools can be retried, run speculatively and auto-approved; mutating ones
    /// generally should not be.
    #[serde(default)]
    pub read_only: bool,
}

/// A model's request to run a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlates this call with its result.
    pub call_id: String,
    /// Which tool to run.
    pub name: ToolName,
    /// The arguments, matching the tool's input schema.
    pub arguments: Value,
}

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// The result, matching the tool's output schema when it declares one.
    pub output: Value,
    /// Whether this represents a failure the model should see and can react to.
    ///
    /// Distinct from returning `Err`: an error the model should reason about (a file that
    /// does not exist) is a successful invocation with `is_error`, whereas `Err` means the
    /// tool could not be run at all.
    #[serde(default)]
    pub is_error: bool,
}

impl ToolOutcome {
    /// A successful outcome.
    pub fn ok(output: impl Into<Value>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
        }
    }

    /// A failure the model should see.
    pub fn error(output: impl Into<Value>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
        }
    }
}

/// Something an agent or a model can invoke.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Describes the tool.
    fn spec(&self) -> ToolSpec;

    /// Runs the tool.
    ///
    /// Implementations must honour `cx`'s cancellation and deadline, and must not enforce
    /// permissions themselves — the runtime has already done so.
    async fn invoke(&self, arguments: Value, cx: &ExecutionContext) -> Result<ToolOutcome>;
}

/// A source of tools.
///
/// A catalogue is how tools become discoverable without every provider being known in
/// advance: a filesystem tool set, an MCP server, a plugin's tools and a user's scripts
/// can each be a catalogue registered under `dyn ToolCatalog`.
#[async_trait]
pub trait ToolCatalog: Send + Sync + 'static {
    /// Lists the tools this catalogue offers.
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ToolSpec>>;

    /// Fetches one tool by name.
    async fn get(&self, name: &ToolName, cx: &ExecutionContext) -> Result<Option<Box<dyn Tool>>>;
}
