//! One server, from the handshake to the calls.
//!
//! A [`McpClient`] owns a server process and knows the three MCP methods this crate
//! uses: `initialize`, `tools/list` and `tools/call`. Everything it returns has already
//! been through [`crate::protocol`], so a caller never sees a server's raw JSON.
//!
//! # Connecting once, lazily
//!
//! A server is started on first use and then kept, rather than started per call. Starting a
//! program and handshaking with it per tool call would put a process spawn inside the
//! latency of every model turn, and would mean a stateful server — one holding an open
//! database connection, a checked-out worktree, a logged-in session — could never be one of
//! these at all.
//!
//! Lazily rather than eagerly, because the alternative is a kernel that will not start
//! when a server is misconfigured or temporarily missing. That trade is the other way round
//! from the rest of this workspace, and it is worth naming: a deployment finds out about a
//! broken tool server the first time it lists tools, not at boot. What is *not* deferred is
//! validation of the deployment's own settings — see [`crate::settings`] — which happens at
//! startup, because that is a mistake an operator made rather than a host that is having a
//! bad day.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_core::{Error, Result};
use serde_json::json;
use tokio::sync::Mutex;

use crate::process::ServerProcess;
use crate::protocol::{self, RemoteToolDefinition, RemoteToolResult, ServerHello};
use crate::settings::{DEFAULT_MAX_LIST_PAGES, ResolvedServer};

/// The name this client reports to a server.
const CLIENT_NAME: &str = "aik";

/// A connected, initialised tool server.
#[derive(Debug)]
struct Connection {
    process: Arc<ServerProcess>,
    hello: ServerHello,
}

/// One MCP server, connected on demand.
pub struct McpClient {
    server: ResolvedServer,
    protocol_version: String,
    connection: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("label", &self.server.label)
            .field("command", &self.server.command)
            .field("protocol_version", &self.protocol_version)
            .finish()
    }
}

impl McpClient {
    /// Creates a client for `server`, without starting anything.
    pub fn new(server: ResolvedServer) -> Self {
        Self {
            server,
            protocol_version: protocol::DEFAULT_PROTOCOL_VERSION.to_owned(),
            connection: Mutex::new(None),
        }
    }

    /// Asks for a different MCP revision than [`protocol::DEFAULT_PROTOCOL_VERSION`].
    ///
    /// What the server *answers* with is still checked against
    /// [`protocol::SUPPORTED_PROTOCOL_VERSIONS`]: this changes what is proposed, not what is
    /// accepted.
    #[must_use]
    pub fn proposing(mut self, version: impl Into<String>) -> Self {
        self.protocol_version = version.into();
        self
    }

    /// The settings this client was built from.
    pub fn server(&self) -> &ResolvedServer {
        &self.server
    }

    /// What the server said about itself, or `None` if it is not running.
    ///
    /// Reported rather than kept private because an operator diagnosing a tool that is not
    /// there needs to know which revision was agreed and which implementation answered, and
    /// the alternative is reading it out of a log line that has scrolled away.
    pub async fn server_info(&self) -> Option<ServerHello> {
        self.connection
            .lock()
            .await
            .as_ref()
            .map(|connection| connection.hello.clone())
    }

    /// Starts and initialises the server if it is not already running.
    ///
    /// Held under one lock for the whole handshake, so two concurrent callers cannot start
    /// two processes — one of which would then be an orphan nobody has a handle to.
    async fn connect(&self, cx: &ExecutionContext) -> Result<Arc<ServerProcess>> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref() {
            return Ok(connection.process.clone());
        }

        let process = ServerProcess::spawn(&self.server)?;
        let hello = match self.handshake(&process, cx).await {
            Ok(hello) => hello,
            Err(error) => {
                // A server that could not be initialised must not be left running: it would
                // hold whatever it opened, and the next call would start a second one.
                process.shutdown().await;
                return Err(error);
            }
        };

        tracing::info!(
            server = %self.server.label,
            protocol = %hello.protocol_version,
            name = hello.server_name.as_deref().unwrap_or("unnamed"),
            "connected to an MCP server"
        );

        *slot = Some(Connection {
            process: process.clone(),
            hello,
        });
        Ok(process)
    }

    /// Performs `initialize` and the notification that completes it.
    async fn handshake(
        &self,
        process: &ServerProcess,
        cx: &ExecutionContext,
    ) -> Result<ServerHello> {
        let result = process
            .session
            .call(
                "initialize",
                protocol::initialize_params(&self.protocol_version, CLIENT_NAME),
                self.server.startup_timeout,
                cx,
            )
            .await?;

        let hello = protocol::parse_hello(&result)?;

        if !hello.serves_tools {
            return Err(Error::Unsupported(format!(
                "MCP server `{}` declares no `tools` capability, so it has nothing this \
                 kernel can expose",
                self.server.label
            )));
        }

        process
            .session
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(hello)
    }

    /// Lists every tool the server offers that this deployment exposes.
    ///
    /// Pagination is followed up to [`DEFAULT_MAX_LIST_PAGES`], and a cursor the server
    /// repeats ends the listing rather than looping: a server that always returns the same
    /// `nextCursor` would otherwise be an infinite listing inside kernel startup.
    ///
    /// The per-server cap is applied to what the server *offers*, before the deployment's
    /// own allowlist narrows it, so a server cannot get past a cap of ten by offering a
    /// thousand tools of which ten are allowlisted.
    pub async fn list_tools(&self, cx: &ExecutionContext) -> Result<Vec<RemoteToolDefinition>> {
        let process = self.connect(cx).await?;

        let mut offered = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors: Vec<String> = Vec::new();

        for _ in 0..DEFAULT_MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = process
                .session
                .call("tools/list", params, self.server.startup_timeout, cx)
                .await?;

            let (page, next) = protocol::parse_tool_list(&result)?;
            offered.extend(page);

            if offered.len() > self.server.max_tools {
                return Err(Error::InvalidArgument(format!(
                    "MCP server `{}` offers more than the {} tools this deployment accepts",
                    self.server.label, self.server.max_tools
                )));
            }

            let Some(next) = next else { break };
            if seen_cursors.contains(&next) {
                tracing::warn!(
                    server = %self.server.label,
                    "stopping a tool listing that repeated its own pagination cursor"
                );
                break;
            }
            seen_cursors.push(next.clone());
            cursor = Some(next);
        }

        let exposed: Vec<RemoteToolDefinition> = offered
            .into_iter()
            .filter(|tool| self.server.exposes(&tool.remote_name))
            .collect();

        // A named tool that the server does not offer is a configuration error an operator
        // should see, not a silently missing capability: it is normally a typo, and the
        // deployment believes it granted something.
        for named in &self.server.tools {
            if !exposed.iter().any(|tool| &tool.remote_name == named) {
                return Err(Error::config(
                    self.server.setting("tools"),
                    format!("`{named}` is not a tool this server offers"),
                ));
            }
        }

        Ok(exposed)
    }

    /// Calls one of the server's tools.
    ///
    /// `remote` is the server's own name for it, not the kernel-side one, and is checked
    /// against the deployment's allowlist here as well as when the catalogue was built —
    /// the second check is cheap and means the allowlist holds even if a future caller
    /// reaches this method by another path.
    pub async fn call_tool(
        &self,
        remote: &str,
        arguments: serde_json::Value,
        cx: &ExecutionContext,
    ) -> Result<RemoteToolResult> {
        if !self.server.exposes(remote) {
            return Err(Error::PermissionDenied(format!(
                "`{remote}` is not a tool MCP server `{}` exposes in this deployment",
                self.server.label
            )));
        }

        let process = self.connect(cx).await?;
        let result = process
            .session
            .call(
                "tools/call",
                json!({ "name": remote, "arguments": arguments }),
                self.server.call_timeout,
                cx,
            )
            .await?;

        Ok(protocol::parse_tool_result(
            &result,
            self.server.max_result_bytes,
        ))
    }

    /// Stops the server, if one is running.
    pub async fn shutdown(&self) {
        let connection = self.connection.lock().await.take();
        if let Some(connection) = connection {
            connection.process.shutdown().await;
        }
    }
}
