//! Every configured server's tools, as one [`ToolCatalog`].
//!
//! This is where an external server's tools become kernel tools, and the three properties
//! that matter are all decided here.
//!
//! # A tool from a server is authorized exactly like any other
//!
//! Nothing in this crate invokes anything. [`McpCatalog`] is a *supply-side* contract: it
//! hands `Box<dyn Tool>` to whatever assembles a [`ToolRegistry`](aik_api::tool::ToolRegistry), and from that point the
//! tool is reached only through the registry's one gated door — the same policy engine, the
//! same approval sink, the same audit events as `fs.read`. An MCP server does not get a
//! second, softer path. See [`aik_api::tool`](aik_api::tool#the-security-boundary).
//!
//! # A server's claims about itself are not authorization inputs
//!
//! MCP lets a tool carry annotations: `readOnlyHint`, `destructiveHint`, `openWorldHint`.
//! They are written by the thing being authorized, so none of them is read here.
//! [`ToolSpec::read_only`] is `false` for every remote tool, always — because that flag is
//! what tells the rest of the system a call is safe to retry, run speculatively or
//! auto-approve, and a server that would like to be auto-approved can say `readOnlyHint`
//! about a tool that deletes things. A deployment that knows a particular server's tools are
//! read-only expresses that in *its* policy, which the server cannot write.
//!
//! # Which tools exist is a deployment decision, twice
//!
//! A server contributes a tool only if the deployment ran that server at all, and only if
//! its `tools` allowlist admits that name. Neither is something policy can widen and neither
//! is something the server can influence. Policy is the inner limit; these are the outer one.
//!
//! # Names
//!
//! `mcp.<server>.<tool>`, where `<server>` is the deployment's own label. A remote name that
//! could punctuate or misrender that is refused in [`crate::protocol`], and a collision with
//! a name already registered is refused by the registry itself — so a server cannot offer a
//! tool that shadows `fs.write`.

use std::collections::HashMap;
use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::provenance::{Reach, Trust};
use aik_api::tool::{ResourceClaim, Tool, ToolCatalog, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::McpClient;
use crate::protocol::RemoteToolDefinition;

/// One exposed tool: which server serves it, and under what name there.
#[derive(Clone)]
struct Entry {
    client: Arc<McpClient>,
    definition: RemoteToolDefinition,
}

/// The tools of every configured MCP server.
pub struct McpCatalog {
    clients: Vec<Arc<McpClient>>,
    /// The listing, resolved once. `None` until the first successful [`McpCatalog::list`].
    listed: Mutex<Option<HashMap<ToolName, Entry>>>,
}

impl std::fmt::Debug for McpCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let labels: Vec<&str> = self
            .clients
            .iter()
            .map(|client| client.server().label.as_str())
            .collect();
        f.debug_struct("McpCatalog")
            .field("servers", &labels)
            .finish()
    }
}

impl McpCatalog {
    /// Creates a catalogue over `clients`, without starting any of them.
    ///
    /// Two servers sharing a label is refused here rather than at registration: the label is
    /// what every one of their tools is named after, so the collision would surface as a
    /// confusing duplicate-tool failure naming neither server.
    pub fn new(clients: Vec<Arc<McpClient>>) -> Result<Self> {
        let mut labels: Vec<&str> = clients
            .iter()
            .map(|client| client.server().label.as_str())
            .collect();
        labels.sort_unstable();
        if let Some(duplicate) = labels.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(Error::already_exists("MCP server label", duplicate[0]));
        }

        Ok(Self {
            clients,
            listed: Mutex::new(None),
        })
    }

    /// The servers this catalogue draws on.
    pub fn clients(&self) -> &[Arc<McpClient>] {
        &self.clients
    }

    /// Stops every running server.
    pub async fn shutdown(&self) {
        for client in &self.clients {
            client.shutdown().await;
        }
        *self.listed.lock().await = None;
    }

    /// Resolves the listing, connecting to any server not yet running.
    ///
    /// Held under one lock so that concurrent callers share one listing rather than each
    /// provoking their own round of `tools/list` calls.
    async fn entries(&self, cx: &ExecutionContext) -> Result<HashMap<ToolName, Entry>> {
        let mut slot = self.listed.lock().await;
        if let Some(listed) = slot.as_ref() {
            return Ok(listed.clone());
        }

        let mut entries: HashMap<ToolName, Entry> = HashMap::new();
        for client in &self.clients {
            let server = client.server();
            for definition in client.list_tools(cx).await? {
                let name = ToolName::new(server.tool_name(&definition.remote_name));
                if entries.contains_key(&name) {
                    return Err(Error::already_exists("MCP tool", &name));
                }
                entries.insert(
                    name,
                    Entry {
                        client: client.clone(),
                        definition,
                    },
                );
            }
        }

        *slot = Some(entries.clone());
        Ok(entries)
    }
}

#[async_trait]
impl ToolCatalog for McpCatalog {
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ToolSpec>> {
        let entries = self.entries(cx).await?;
        let mut specs: Vec<ToolSpec> = entries
            .iter()
            .map(|(name, entry)| spec_for(name, entry))
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs)
    }

    async fn get(&self, name: &ToolName, cx: &ExecutionContext) -> Result<Option<Box<dyn Tool>>> {
        let entries = self.entries(cx).await?;
        Ok(entries.get(name).map(|entry| {
            Box::new(RemoteTool {
                name: name.clone(),
                client: entry.client.clone(),
                definition: entry.definition.clone(),
            }) as Box<dyn Tool>
        }))
    }
}

/// Builds the specification a model is shown for one remote tool.
fn spec_for(name: &ToolName, entry: &Entry) -> ToolSpec {
    let server = entry.client.server();
    ToolSpec {
        name: name.clone(),
        description: entry.definition.description.clone(),
        input_schema: entry.definition.input_schema.clone(),
        output_schema: entry.definition.output_schema.clone(),
        required_permissions: vec![ActionId::new(server.permission.clone())],
        // Never taken from the server's own annotations: see the module documentation.
        read_only: false,
        // Authored by a server this repository did not write, exactly like the description
        // and the schema above. Never taken from the server's own annotations.
        output_trust: Trust::Untrusted,
        // A server can do anything the program does, and this kernel cannot see which.
        reach: Reach::External,
    }
}

/// One tool, served by a process this kernel started.
struct RemoteTool {
    name: ToolName,
    client: Arc<McpClient>,
    definition: RemoteToolDefinition,
}

impl std::fmt::Debug for RemoteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTool")
            .field("name", &self.name)
            .field("remote_name", &self.definition.remote_name)
            .finish()
    }
}

#[async_trait]
impl Tool for RemoteTool {
    fn spec(&self) -> ToolSpec {
        spec_for(
            &self.name,
            &Entry {
                client: self.client.clone(),
                definition: self.definition.clone(),
            },
        )
    }

    /// Declares the one resource this call acts on: the tool itself.
    ///
    /// Not the arguments. A filesystem tool can canonicalise a path because it knows which
    /// argument is one and what it will do with it; nothing here knows what a third-party
    /// server's `query` or `target` field denotes, and a registry that pattern-matched on
    /// the raw string would authorize the literal text rather than the thing. Claiming a
    /// resource this crate cannot canonicalise would be worse than claiming none: it would
    /// put a decision in the audit trail that does not mean what it says.
    ///
    /// So the resource is `mcp:<server>/<tool>` — a thing this crate does know, is stable,
    /// and is what a policy rule can usefully be written against. Authorization *within* a
    /// server's tool, over the arguments, is the server's own business, and a deployment
    /// that needs it should not be exposing that tool to a model.
    fn planned_resources(&self, _arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let server = self.client.server();
        Ok(vec![ResourceClaim::new(
            ActionId::new(server.permission.clone()),
            ResourceId::new(server.resource_id(&self.definition.remote_name)),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        // MCP says a call's arguments are an object. A model that sends `null` — which is
        // what an empty tool call looks like from several providers — means "no arguments",
        // and turning that into `{}` here is a translation rather than a decision. Anything
        // else is refused as an argument error the model can see and correct, rather than
        // forwarded for the server to reject in whatever way it chooses.
        let arguments = match arguments {
            Value::Null => Value::Object(serde_json::Map::new()),
            object @ Value::Object(_) => object,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "`{}` takes an object of arguments, not a {}",
                    self.name,
                    kind_of(&other)
                )));
            }
        };

        let result = self
            .client
            .call_tool(&self.definition.remote_name, arguments, cx)
            .await?;

        // Belt and braces over the specification's own `output_trust`: this is a reply from
        // a server whose code nobody here reviewed, whichever way the specification was
        // built.
        Ok(ToolOutcome {
            output: result.output,
            is_error: result.is_error,
            trust: Trust::Untrusted,
        })
    }
}

/// Names a JSON value's type for an error message.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ServerSettings;

    fn client(label: &str) -> Arc<McpClient> {
        let settings = ServerSettings {
            label: label.to_owned(),
            command: "server".to_owned(),
            ..ServerSettings::default()
        };
        Arc::new(McpClient::new(
            settings.resolve(std::path::Path::new("/tmp")).unwrap(),
        ))
    }

    #[test]
    fn two_servers_sharing_a_label_are_refused_before_anything_starts() {
        // The failure this rules out is two servers both contributing `mcp.files.read`, which
        // would surface as a duplicate-tool error naming neither of them.
        let error = McpCatalog::new(vec![client("files"), client("files")]).unwrap_err();
        assert_eq!(error.kind(), aik_core::ErrorKind::Conflict);
    }

    #[test]
    fn distinct_labels_are_accepted() {
        McpCatalog::new(vec![client("files"), client("issues")]).unwrap();
    }
}
