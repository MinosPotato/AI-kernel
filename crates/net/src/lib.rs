//! An HTTP fetch [`Tool`](aik_api::tool::Tool), confined to addresses a deployment allows.
//!
//! This is the second place the kernel reaches outside this machine, and the first where
//! *the model chooses where*. `aik-anthropic` and `aik-openai` also make network calls, but
//! to one endpoint an operator configured; here the destination is an argument, produced by
//! a model, from a conversation that may contain text somebody else wrote. That inverts the
//! question this crate has to answer. It is not "can we reach the network" — it is "given
//! that the address is attacker-influenced, what may it be".
//!
//! # Why an unconstrained fetch is a privilege escalation
//!
//! A process that can make an HTTP request on somebody's behalf is a proxy into every
//! network that process sits in. The classic consequences are not exotic:
//!
//! * `http://169.254.169.254/…` answers, on most hosted machines, with credentials for the
//!   role the machine runs as.
//! * `http://127.0.0.1:…` and the RFC 1918 ranges reach the admin interfaces, databases and
//!   internal APIs that are unauthenticated *because* they are unreachable from outside.
//! * A name whose DNS record changes between the check and the connection reaches all of
//!   the above while every check written against the name passes.
//! * A redirect reaches all of the above without the request ever having named it.
//!
//! None of those is a policy question in the first instance. A deployment that wrote
//! `allow web.fetch` meant "the model may read the web", and every one of the above is a
//! way of turning that sentence into something else. So this crate enforces a boundary of
//! its own, exactly as `aik-fs` confines a path to a root regardless of what policy says:
//! authorization decides *whether a principal may fetch a URL at all*, and this crate
//! decides, independently and unconditionally, *what a URL is allowed to be*. A permissive
//! policy can only narrow what these checks allow, never widen it.
//!
//! # The four boundaries
//!
//! | Boundary | Where | What it stops |
//! |---|---|---|
//! | Shape | `target.rs` | Non-HTTP schemes, credentials in the URL, privileged ports, hosts a deployment excluded |
//! | Address | [`address`] | Loopback, private and carrier-grade-NAT ranges unless opted in; metadata, multicast, broadcast and IPv4-in-IPv6 forms always |
//! | Resolution | `resolver.rs` | A record that changes between the check and the connection |
//! | Response | [`WebFetchTool`] | Unbounded bodies, non-text content, unauthorized redirect hops |
//!
//! Each is independent: none of them is the reason another one is safe, and none of them is
//! reached around by turning another off. The one setting that relaxes an address check
//! ([`NetSettings::allow_local_addresses`]) admits this machine's own networks and *not* the
//! link-local range where instance credentials answer, because a deployment that meant to
//! reach a wiki at `10.0.0.5` did not thereby mean to hand out its own role.
//!
//! # What comes back is not trustworthy
//!
//! Everything this tool returns was written by whoever runs the server that answered. It
//! reaches a model as tool output, which is exactly the position `aik-mcp` puts a third
//! party's tool results in, and it gets the same treatment: parsed narrowly, bounded
//! everywhere, decoded as one encoding rather than whatever was declared, and reduced to
//! text before a model sees it. The tool's own description says so to the model in as many
//! words. What this crate cannot do is make a fetched page not contain instructions —
//! nothing at this layer can — so the guarantee it does provide is the one that matters
//! structurally: a page that tells a model to do something reaches
//! [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke) if the model believes it,
//! and policy, approval and the audit trail are all still in front of whatever it asks for.
//! Fetching a page grants nothing.
//!
//! # Usage
//!
//! ```no_run
//! # fn main() -> aik_core::Result<()> {
//! use aik_net::{NetSettings, WebFetchTool};
//!
//! let tool = WebFetchTool::new(NetSettings {
//!     allow_hosts: vec![".rust-lang.org".to_owned()],
//!     ..NetSettings::default()
//! })?;
//! # let _ = tool;
//! # Ok(())
//! # }
//! ```
//!
//! The tool is then registered with a
//! [`ToolRegistry`](aik_api::tool::ToolRegistry) by trusted code — in this workspace, by
//! `aik-runtime` — and reached only through it.

pub mod address;
mod extract;
mod resolver;
mod settings;
mod target;
mod tool;

pub use settings::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_MAX_BYTES, DEFAULT_MAX_REDIRECTS, DEFAULT_NAME,
    DEFAULT_PERMISSION, DEFAULT_TIMEOUT, DEFAULT_USER_AGENT, HOST_RESOURCE_PREFIX, MAX_URL_BYTES,
    NetSettings, URL_RESOURCE_PREFIX,
};
pub use tool::WebFetchTool;
