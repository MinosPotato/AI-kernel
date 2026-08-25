//! A process-execution [`Tool`](aik_api::tool::Tool), confined by an OS-level sandbox.
//!
//! This is the first capability in the kernel where the thing being authorized is not a
//! request the implementation carries out, but *arbitrary host code the implementation
//! starts*. Everything `aik-fs` does, it does itself: it resolves a path, checks the result
//! against a root, opens a handle, reads bytes. Nothing it hands the host can decide to do
//! something else. A program can. `git` allowlisted here is not a promise to read a
//! repository — it is a promise to run whatever `/usr/bin/git` is, which reads configuration
//! files, may run hooks, and can be handed arguments that make it write anywhere the account
//! can write.
//!
//! That difference is why this crate is shaped the way it is, and why one of its four
//! measures is not like the others.
//!
//! # Four measures, and only one of them is a boundary
//!
//! | Measure | Answers | Bounds |
//! |---|---|---|
//! | Registration | Does this deployment execute anything at all? | What exists |
//! | The allowlist ([`ExecTool::new`]) | Which programs? | What is asked for |
//! | Policy ([`ResourceClaim`](aik_api::tool::ResourceClaim)s) | Which commands, for whom? | What is asked for |
//! | The [`Sandbox`] | What can it reach once running? | **What happens** |
//!
//! The first three are cooperative, and they are worth having: they are the difference
//! between a model that can ask for `git status` and one that can ask for anything, and they
//! are what an audit trail records and a human approves. But none of them survives contact
//! with a program that does something other than what its name suggests. Only the sandbox
//! does, which is why [`Sandbox::bubblewrap`] verifies at *startup* that the host can actually
//! provide one, and why the alternative is named [`Sandbox::Unconfined`] rather than
//! something that sounds like a lighter kind of confinement.
//!
//! # What a sandboxed child gets
//!
//! Its own user, mount, pid, ipc and uts namespaces; a read-only view of `/usr` and the
//! loader's files, and *nothing else of `/etc`*; a private `/proc`, a minimal `/dev`, and a
//! size-capped tmpfs for `/tmp`; no network unless the deployment granted one; the workspace,
//! read-only by default, as the single writable path; an environment built from nothing; a
//! session of its own with no terminal; and resource limits the OS enforces whatever this
//! process does afterwards. See [`sandbox`] and [`limits`].
//!
//! # There is no shell, anywhere
//!
//! A call supplies a program name and a vector of arguments. There is no place in this crate
//! where a string becomes a command line: nothing is split on whitespace, glob-expanded, or
//! passed to `sh -c`. An argument containing `; rm -rf /` is one argument containing those
//! characters. The single rendering of a command *as* a line — the string a policy rule
//! matches — exists only
//! to name a call for a policy rule and a human, is injective so that no two different calls
//! can share a name, and is never parsed back.
//!
//! # Example
//!
//! ```no_run
//! use aik_exec::{ExecTool, Sandbox};
//!
//! # fn main() -> aik_core::Result<()> {
//! let tool = ExecTool::new("/home/user/project", Sandbox::bubblewrap()?, ["git", "rg"])?
//!     .with_timeout(std::time::Duration::from_secs(15));
//! # let _ = tool;
//! # Ok(())
//! # }
//! ```
//!
//! The tool is then registered with a [`ToolRegistry`](aik_api::tool::ToolRegistry)
//! implementation, which is the only thing that can reach it — see
//! [`aik_api::tool`](aik_api::tool#the-security-boundary).

mod command_line;
pub mod limits;
mod program;
mod runner;
pub mod sandbox;
mod tool;

pub use limits::Limits;
pub use sandbox::{DEFAULT_TMPFS_BYTES, SANDBOX_SEARCH_PATH, Sandbox, WORKSPACE};
pub use tool::{
    COMMAND_RESOURCE_PREFIX, DEFAULT_MAX_ARGUMENT_BYTES, DEFAULT_MAX_ARGUMENTS,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_STDIN_BYTES, DEFAULT_NAME, DEFAULT_PERMISSION,
    DEFAULT_SEARCH_PATH, DEFAULT_TIMEOUT, ExecTool, PROGRAM_RESOURCE_PREFIX,
};
