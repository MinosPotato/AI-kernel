//! Filesystem [`Tool`](aik_api::tool::Tool)s, each confined to a configured root directory.
//!
//! These are the first tools in the kernel that touch the host system, so this is the first
//! place the tool/authorization foundation (`aik-api`, `aik-tools`) meets capabilities that
//! can actually do something to a real machine. Two independent things keep that safe, and
//! neither is optional:
//!
//! 1. **Authorization** (`aik-api`, `aik-tools`, and typically a policy engine such as
//!    [`aik-policy`](https://docs.rs/aik-policy)) decides *whether a principal may touch a
//!    given path at all*. This crate contributes to that question — each tool declares the
//!    canonical path it intends to act on as a
//!    [`ResourceClaim`](aik_api::tool::ResourceClaim), so a policy can express "allow reads
//!    under `/home/user/project`, deny under `secrets/`" — but it does not answer the
//!    question itself. See [`aik_api::tool`] for why that boundary belongs to the registry,
//!    not the tool.
//! 2. **Enforcement** (this crate) independently guarantees that no matter what policy
//!    decides, a tool never touches anything outside its configured root. A permissive or
//!    misconfigured policy engine can only ever *narrow* what these tools will do, never
//!    widen it. See each tool's documentation for exactly how paths are resolved and
//!    confined, and what that does and does not guarantee against a concurrent, adversarial
//!    filesystem.
//!
//! # The two capabilities are separate on purpose
//!
//! [`FsReadTool`] and [`FsWriteTool`] are distinct tools requiring distinct permissions
//! ([`DEFAULT_PERMISSION`] and [`DEFAULT_WRITE_PERMISSION`]), and are registered
//! independently. A deployment that wants an agent to read its project and not change it
//! simply does not register the write tool — or registers it and denies
//! `filesystem.write` in policy, which is the same guarantee obtained a second, independent
//! way. Nothing about holding one capability implies the other, and the two tools share no
//! state: even pointed at the same root, neither can act through the other.
//!
//! Confinement itself is shared, because it must be: a read and a write given the same root
//! and the same argument resolve that argument through the same code, so the two boundaries
//! cannot drift apart.

mod common;
mod tool;
mod write;

pub use tool::{DEFAULT_MAX_BYTES, DEFAULT_NAME, DEFAULT_PERMISSION, DEFAULT_TIMEOUT, FsReadTool};
pub use write::{
    DEFAULT_CREATE_MODE, DEFAULT_MAX_WRITE_BYTES, DEFAULT_WRITE_NAME, DEFAULT_WRITE_PERMISSION,
    FsWriteTool,
};
