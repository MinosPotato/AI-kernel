//! A Model Context Protocol client: external tool servers, behind the kernel's own gate.
//!
//! This crate is the second thing in the workspace that supplies tools, and the first that
//! supplies tools it did not write. `aik-fs` and `aik-exec` are implementations of
//! [`Tool`](aik_api::tool::Tool) that this repository owns, reviewed here, changing only
//! when somebody changes them. An MCP server is a program the operator points at: its tool
//! names, its descriptions, its schemas and its results are all authored elsewhere and can
//! change between one start and the next.
//!
//! That is what makes it worth having. `aik-anthropic` exists so that
//! [`ModelProvider`](aik_api::model::ModelProvider) is a contract rather than a description
//! of Ollama; this crate does the same for
//! [`ToolCatalog`](aik_api::tool::ToolCatalog) — which named an MCP server in its
//! documentation before one existed — and it is also the point at which the tool contract
//! stops being satisfied only by code that already trusted itself.
//!
//! # Where the trust boundary is
//!
//! Two boundaries, and they are not the same one.
//!
//! **A server is trusted code.** It runs as the account the kernel runs as, and this crate
//! does not sandbox it — see [`process`] for why, and for what that does and does not mean.
//! Running a particular server is a deployment decision an operator makes in configuration.
//! A model cannot add one, name one, or influence which one is started.
//!
//! **A server's output is not.** Everything a server writes reaches a model, and in a
//! deployment where the server talks to the outside world — an issue tracker, a web page, a
//! shared database — that output is written by whoever can reach the thing it talks to. So
//! it is parsed narrowly and bounded everywhere: names are validated, descriptions are
//! stripped and capped, schemas are checked for the shape a provider needs, results are
//! truncated, binary content is described rather than carried, and frames are size-limited
//! before they are parsed. See [`protocol`].
//!
//! # Four limits, and only one of them is policy
//!
//! | Limit | Answers | Set by |
//! |---|---|---|
//! | Which servers run | Is there an MCP tool at all? | Configuration, at startup |
//! | A server's `tools` allowlist | Which of its tools exist here? | Configuration, at startup |
//! | Policy | Who may call which, and when is a human asked? | The policy document |
//! | Frames, results, tool counts | What may one call cost? | Configuration, with conservative defaults |
//!
//! The first two are the outer limit and cannot be widened by policy; policy is the inner
//! one and cannot be widened by a server. A tool that is not registered is unreachable
//! however permissive the policy is, and a tool that is registered still passes through the
//! same [`ToolRegistry`](aik_api::tool::ToolRegistry) door as `fs.write`.
//!
//! # What a server may not do
//!
//! MCP is bidirectional. A server can ask a client to sample from its model, to reveal its
//! filesystem roots, to prompt its user. This client advertises **no capabilities**, so
//! every such request is answered with "method not found": a tool call cannot become a model
//! call the deployment pays for, with a prompt the server wrote, and a server cannot learn
//! where on the host this deployment keeps its files. A server also cannot make itself
//! auto-approvable — MCP's `readOnlyHint` and friends are claims by the thing being
//! authorized, and none of them is read. See [`catalog`].
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use aik_mcp::{McpCatalog, McpClient, McpComponent, ServerSettings};
//!
//! # fn wire() -> aik_core::Result<()> {
//! let settings = ServerSettings {
//!     label: "files".into(),
//!     command: "mcp-server-filesystem".into(),
//!     args: vec!["/home/user/project".into()],
//!     ..ServerSettings::default()
//! };
//! let client = McpClient::new(settings.resolve(std::path::Path::new("/home/user/project"))?);
//! let catalog = Arc::new(McpCatalog::new(vec![Arc::new(client)])?);
//!
//! // The catalogue goes to whatever assembles the tool registry, and this component owns
//! // the servers' lifetime.
//! let _component = McpComponent::new(catalog.clone());
//! # Ok(())
//! # }
//! ```

pub mod catalog;
pub mod client;
mod component;
pub mod process;
pub mod protocol;
pub mod session;
pub mod settings;

pub use catalog::McpCatalog;
pub use client::McpClient;
pub use component::{DEFAULT_COMPONENT_ID, McpComponent};
pub use protocol::{
    DEFAULT_PROTOCOL_VERSION, RemoteToolDefinition, RemoteToolResult, SUPPORTED_PROTOCOL_VERSIONS,
    ServerHello,
};
pub use settings::{
    DEFAULT_CALL_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_RESULT_BYTES, DEFAULT_MAX_TOOLS,
    DEFAULT_PERMISSION, DEFAULT_SEARCH_PATH, DEFAULT_SETTINGS_PATH, DEFAULT_STARTUP_TIMEOUT,
    NAME_PREFIX, RESOURCE_PREFIX, ResolvedServer, ServerSettings,
};
