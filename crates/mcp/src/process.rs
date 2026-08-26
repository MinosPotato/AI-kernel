//! Turning a configured program into the two streams a JSON-RPC session speaks over.
//!
//! Everything security-relevant about starting the child is here, and there is not much of
//! it, because the interesting decisions were already made:
//!
//! * **The name is not a path.** Which file `uvx` means is decided by
//!   [`aik_exec::program`], on a *configured* search path, never on the process `PATH` this
//!   kernel inherited. That is the same resolution the process-execution tool uses, called
//!   rather than copied.
//! * **The environment is built from nothing.** `env_clear` first, then exactly what the
//!   deployment wrote down. A tool server is third-party code; it does not get this
//!   kernel's model credential, its database path, or its operator's `SSH_AUTH_SOCK`
//!   because nobody thought about it.
//! * **Standard input and output are the protocol.** Neither is inherited. A child sharing
//!   this process's standard input could read whatever the operator types next, including
//!   the answer to the approval prompt for the call it is serving.
//! * **Standard error is drained.** A pipe nobody reads fills, and a server that blocks
//!   writing a diagnostic stops answering — the classic deadlock, reached here by a server
//!   that logs. It is read continuously and forwarded to `tracing` at a bounded line length.
//! * **The child leads its own process group.** So closing the session kills the tree it
//!   started, not only the process this crate holds a handle to.
//!
//! # What this is not
//!
//! There is no sandbox. A tool server runs with the privileges of the account the kernel
//! runs as, and the boundary around what it can reach is the OS's, not this crate's. That
//! is a deliberate limit rather than an oversight: a server exists to do something outside
//! this process — read a repository, query a database, call an API — and a confinement
//! tight enough to be worth calling one would have to be written per server by whoever
//! knows what that server needs. What this crate bounds is the *protocol*: which tools
//! reach a model, what a call may cost, and what a result may contain. Which servers run at
//! all is a deployment decision, made once, in configuration, by an operator — never by a
//! model and never by a conversation.

use std::process::Stdio;
use std::sync::Arc;

use aik_core::{Error, Result};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::session::{Session, Sink, Source};
use crate::settings::ResolvedServer;

/// The longest line of a server's standard error that is logged.
const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;

/// A running tool server and the session speaking to it.
#[derive(Debug)]
pub(crate) struct ServerProcess {
    /// The conversation.
    pub(crate) session: Session,
    /// The child, kept so that it can be killed and reaped.
    child: Mutex<Option<Child>>,
    /// The child's pid, captured before it can be reaped — which is what names the process
    /// group the whole tree is in.
    pid: Option<u32>,
    /// The server's label, for messages.
    label: String,
}

impl ServerProcess {
    /// Starts `server` and begins reading its output.
    ///
    /// Failing to start is a plain error rather than a panic or a retry: a server the host
    /// does not have is a configuration problem an operator has to see.
    pub(crate) fn spawn(server: &ResolvedServer) -> Result<Arc<Self>> {
        let program =
            aik_exec::program::resolve(&server.command, &server.search_path).map_err(|error| {
                Error::config(
                    server.setting("command"),
                    format!(
                        "`{}` was not found on the configured search path: {error}",
                        server.command
                    ),
                )
            })?;

        let mut command = Command::new(&program);
        command
            .args(&server.args)
            .env_clear()
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dropped handle — a panic anywhere above, a cancelled task — must not leave
            // the server running. This covers the paths the explicit kill does not reach.
            .kill_on_drop(true);

        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        }

        // SAFETY: the closure is `aik_exec::limits::apply`, which documents every call it
        // makes as async-signal-safe. No limits are lowered here — a tool server is
        // long-lived and legitimately needs memory and descriptors — so what this buys is
        // the session of its own that makes the group kill below cover the whole tree.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| aik_exec::limits::apply(&NO_LIMITS));
        }

        let mut child = command.spawn().map_err(|error| {
            Error::wrap(
                format!(
                    "starting MCP server `{}` ({})",
                    server.label,
                    program.display()
                ),
                error,
            )
        })?;

        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            Error::other(format!(
                "MCP server `{}` was started without a standard input",
                server.label
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::other(format!(
                "MCP server `{}` was started without a standard output",
                server.label
            ))
        })?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_diagnostics(server.label.clone(), stderr));
        }

        let session = Session::start(
            server.label.clone(),
            Box::new(stdout) as Source,
            Box::new(stdin) as Sink,
            server.max_frame_bytes,
        );

        Ok(Arc::new(Self {
            session,
            child: Mutex::new(Some(child)),
            pid,
            label: server.label.clone(),
        }))
    }

    /// Ends the session and the process tree behind it.
    ///
    /// Idempotent: shutting down twice, or shutting down a server that has already exited,
    /// is not an error. The group is signalled *before* the handle is killed, so a server
    /// that forked helpers does not leave them holding the pipes open.
    pub(crate) async fn shutdown(&self) {
        self.session.close().await;

        let Some(mut child) = self.child.lock().await.take() else {
            return;
        };

        #[cfg(unix)]
        if let Some(pid) = self.pid {
            aik_exec::limits::kill_group(pid);
        }

        let _ = child.start_kill();
        match child.wait().await {
            Ok(_) => {}
            Err(error) => tracing::warn!(
                server = %self.label,
                %error,
                "could not reap an MCP server after killing it"
            ),
        }
    }
}

/// No resource limits at all: see the `pre_exec` call above for why.
#[cfg(unix)]
static NO_LIMITS: aik_exec::Limits = aik_exec::Limits {
    cpu_seconds: None,
    file_size_bytes: None,
    open_files: None,
    address_space_bytes: None,
    processes: None,
};

/// Reads a server's standard error forever, so the pipe never fills.
///
/// Bounded per line, and the overflow is discarded as it arrives rather than buffered: a
/// server that writes a megabyte without a newline would otherwise put a megabyte in the
/// operator's log, and one that never writes a newline at all would put it in memory.
async fn drain_diagnostics(label: String, stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line: Vec<u8> = Vec::new();

    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) => {
                tracing::debug!(server = %label, %error, "stopped reading an MCP server's diagnostics");
                return;
            }
        };
        if available.is_empty() {
            break;
        }

        let (chunk, consumed, complete) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&available[..end], end + 1, true),
            None => (available, available.len(), false),
        };
        let room = MAX_DIAGNOSTIC_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..room.min(chunk.len())]);
        reader.consume(consumed);

        if complete {
            log_diagnostic(&label, &line);
            line.clear();
        }
    }

    if !line.is_empty() {
        log_diagnostic(&label, &line);
    }
}

/// Logs one bounded diagnostic line, stripped of anything that would misrepresent it.
fn log_diagnostic(label: &str, line: &[u8]) {
    let text = crate::protocol::sanitize(&String::from_utf8_lossy(line), MAX_DIAGNOSTIC_BYTES);
    if !text.trim().is_empty() {
        tracing::debug!(server = %label, "{text}");
    }
}
