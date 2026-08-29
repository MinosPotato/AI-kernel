//! Tool contracts.
//!
//! A tool is a named, schema-described capability that a model or an agent can invoke.
//! Inputs and outputs are JSON described by JSON Schema, because that is what model
//! providers expect and what can cross a process or sandbox boundary unchanged.
//!
//! [`ToolSpec::required_permissions`] is the link to the [permission](crate::permission)
//! layer: a tool declares what it needs, but never decides whether it gets it. That
//! decision — and the only path to actually running a tool — belongs to
//! [`ToolRegistry`], not to the tool itself. See its documentation for why.
//!
//! # The security boundary
//!
//! The conceptual flow this crate is built around is:
//!
//! ```text
//! model output
//!   → tool request
//!     → tool-level authorization      (may this principal use filesystem.write?)
//!       → resource-level authorization (…on /home/user/project/file.rs?)
//!         → tool execution
//!           → audit event
//! ```
//!
//! A model asking for something, or claiming it is allowed to do it, has **zero**
//! authorization significance. The only question that matters is: given who is actually
//! asking (`ExecutionContext::principal`) and what they are actually asking for
//! (`ToolSpec::required_permissions`), does policy allow it? Nothing about the *content*
//! of a model's output — including a model asserting its own authority — feeds into that
//! question.
//!
//! Four places could plausibly enforce this. In order of increasing centralisation:
//!
//! 1. **Tool-level.** Each [`Tool::invoke`] checks permissions itself before acting.
//!    Rejected: it requires trusting every tool implementation to remember, and to get
//!    it right, with no way to verify from outside that any of them actually did. One
//!    careless or malicious tool silently breaks the guarantee for the whole system.
//! 2. **[`ExecutionContext`]-level.** Bake a permission check into the context object
//!    itself. Rejected: `ExecutionContext` is inert, shared data — correlation, principal,
//!    deadline, cancellation — used by every subsystem, not just tools. Giving it the
//!    ability to *decide* things would turn a passive carrier into an active policy
//!    component, and couple every consumer of it to however authorization happens to be
//!    wired. `ExecutionContext` supplies the *who* (`principal`); it must not supply the
//!    *may they*.
//! 3. **A separate authorization service.** [`PolicyEngine`](crate::permission::PolicyEngine)
//!    already exists as exactly this: a rules engine that turns a
//!    [`PermissionRequest`](crate::permission::PermissionRequest) into a
//!    [`Decision`](crate::permission::Decision), with no opinion about tools, models, or
//!    anything else. This is necessary, but not sufficient on its own — something still has
//!    to guarantee it is *always consulted*.
//! 4. **Registry-level.** [`ToolRegistry`] is the only thing an agent is ever given a
//!    handle to; it never hands back a `dyn Tool`. That makes it the single code path that
//!    can reach [`Tool::invoke`], and that path always resolves
//!    [`ToolSpec::required_permissions`] against a [`PolicyEngine`](crate::permission::PolicyEngine)
//!    first. This is (3) plus the structural guarantee that (3) is unavoidable.
//!
//! (4), built on top of (3), is the smallest design that provides a boundary that does not
//! depend on every future tool author getting it right: **never construct a `dyn Tool`
//! anywhere an agent can reach it.** A tool implementation is a private detail owned by
//! whatever registers it; the only public surface is [`ToolRegistry::list`] (discovery —
//! seeing what exists is not doing it) and [`ToolRegistry::invoke`] (the one gated door).
//!
//! This also means [`aik_core::Registry`] — the kernel's general dependency-injection
//! container — must never be used to store individual `Arc<dyn Tool>` values. That registry
//! is deliberately open: anything holding a `KernelContext` or `ComponentContext` can
//! resolve any capability registered in it. That openness is exactly right for swappable
//! infrastructure like `dyn ModelProvider`, and exactly wrong for something that must only
//! ever be reached through an authorization check. A concrete [`ToolRegistry`]
//! implementation keeps its tools in storage of its own, not the kernel's.
//!
//! # Resource-level authorization
//!
//! Capability-level authorization is not enough on its own. `filesystem.write` is a
//! meaningful thing to grant an agent inside a project directory and a catastrophic thing
//! to grant it over `/etc`, and no amount of care about *who* is asking distinguishes those
//! two if the question never mentions *what* is being written.
//!
//! Four designs were considered for asking the second question:
//!
//! 1. **The registry extracts resources from arguments.** It would need a declarative
//!    mapping (say, a JSON pointer per tool) from arguments to resource identifiers.
//!    Rejected: extraction is not the hard part, *canonicalisation* is. Deciding that
//!    `project/../../../etc/shadow` denotes `/etc/shadow` requires knowing the argument is
//!    a path, what it is relative to, and how the host resolves symlinks — domain knowledge
//!    the registry cannot have for arbitrary tools. A registry that pattern-matched on the
//!    raw string would authorize the literal text and be trivially defeated.
//! 2. **The tool declares its resources before execution.** [`Tool::planned_resources`]
//!    turns arguments into [`ResourceClaim`]s, which the registry authorizes before the
//!    tool runs. Domain knowledge stays in the tool, the *decision* stays in the registry,
//!    and a refusal means nothing executed at all.
//! 3. **An authorization service is handed to the tool.** The tool asks about resources as
//!    it encounters them. Strictly weaker than (2) on its own — nothing structurally
//!    guarantees the tool asks — but it is the only thing that can cover resources that
//!    are not knowable from the arguments.
//! 4. **Two-phase: (2) then (3).** What is implemented.
//!
//! (2) alone would be the smaller design, and for the common case it is the whole story.
//! It is not sufficient, because of the next section: a tool that resolves a path and finds
//! it now points somewhere else has no way to report that, and its only remaining options
//! are to proceed unauthorized or to fail. So (3) is present too — as the *same* mechanism,
//! not a parallel one. The [`ResourceAuthorizer`] the registry passes to
//! [`Tool::invoke`] is the identical object the registry used for phase two, bound to the
//! same principal, policy engine, correlation id and audit stream. There is one
//! authorization path; phases differ only in who initiates the question.
//!
//! Handing a tool the ability to *ask* is not the same as moving policy into the tool. The
//! tool never sees a rule, never sees a [`Decision`](crate::permission::Decision), and
//! cannot answer its own question — it learns only that it may proceed, or gets
//! [`Error::PermissionDenied`](aik_core::Error::PermissionDenied).
//!
//! # Time-of-check to time-of-use
//!
//! **The architecture cannot make resource authorization TOCTOU-safe on its own, and no
//! architecture at this layer could.** This is a property of what a [`ResourceId`] is: a
//! *name*, not a handle to a kernel object. Between authorizing the name
//! `/home/user/project/file.rs` and the `write(2)` that lands on it, anything with write
//! access to that directory can replace the file with a symlink to `/etc/shadow`. The
//! decision was sound; the thing it was made about changed underneath it.
//!
//! What the kernel provides is the decision points. Binding those decisions to actual
//! objects is necessarily the tool's job, because only the tool makes the syscalls:
//!
//! * **Canonicalise before declaring.** [`Tool::planned_resources`] must resolve the
//!   resource to the form policy is written against, so the check is not defeated by
//!   spelling.
//! * **Operate on handles, not names.** Resolve once, then act through the resulting
//!   handle — `openat` with `O_NOFOLLOW`, an already-open file descriptor, a connection
//!   already established to a checked address. A tool that authorizes a path and then
//!   independently re-opens it by name has re-introduced the race it just avoided.
//! * **Re-authorize what you actually found.** If resolution reveals a different object
//!   than the one declared, ask again through the [`ResourceAuthorizer`] before touching
//!   it. This is the case phase three exists for.
//! * **Sandbox for hard guarantees.** Cooperative checks bound what a *correct* tool does.
//!   They do not bound a buggy or hostile one; only an enforcement boundary the tool cannot
//!   reach around — a container, a namespace, seccomp — does that. That belongs in the
//!   tool's execution environment, not in this contract.
//!
//! The same limit applies to [`Tool::planned_resources`] generally: the registry guarantees
//! a decision is *made* about every declared resource, not that the tool then confines
//! itself to them. A tool is trusted code. What the boundary buys is that untrusted input —
//! model output — cannot reach a tool without a decision having been made, and that every
//! such decision is recorded.

use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;
use crate::permission::{ActionId, ResourceAuthorizer, ResourceId};
use crate::provenance::{Reach, Trust};

aik_core::string_id! {
    /// Names a tool, e.g. `fs.read` or `hyprland.focus_window`.
    pub ToolName
}

/// A specific resource a tool intends to act on, and what it intends to do to it.
///
/// This is what turns "may this principal use `filesystem.write`?" into "may this
/// principal use `filesystem.write` on `/home/user/project/file.rs`?". The generic model
/// knows nothing about paths: a [`ResourceId`] is an opaque string whose meaning belongs
/// entirely to whoever writes the tool and the policy that governs it. A window id, a
/// repository ref, a URL and a database table are all equally expressible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceClaim {
    /// The capability being exercised, normally one of the tool's
    /// [`ToolSpec::required_permissions`].
    pub action: ActionId,
    /// The specific target.
    pub resource: ResourceId,
}

impl ResourceClaim {
    /// Declares an intent to perform `action` on `resource`.
    pub fn new(action: impl Into<ActionId>, resource: impl Into<ResourceId>) -> Self {
        Self {
            action: action.into(),
            resource: resource.into(),
        }
    }
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
    /// What this tool's output is, in [trust](crate::provenance) terms.
    ///
    /// [`Trust::Untrusted`] means the output can carry text a third party wrote — a fetched
    /// page, a file somebody else can write to, a program's stdout, an external server's
    /// reply. It is a statement about the *channel*, not about any one result: a tool whose
    /// output is usually this deployment's own but sometimes is not declares the worse case
    /// here and narrows it per call with [`ToolOutcome::with_trust`].
    ///
    /// The default is [`Trust::Untrusted`], and the default is what a specification
    /// *deserialised* from somewhere gets. That is the fail-closed direction: a tool
    /// described by an external server has no way to declare its output trustworthy, and a
    /// field added to this struct later must not silently promote anything.
    #[serde(default = "Trust::untrusted")]
    pub output_trust: Trust,
    /// How far this tool's effects travel — the question that decides whether a conversation
    /// that has read untrusted content may use it at all.
    ///
    /// Defaults to [`Reach::External`], the widest, for the same reason
    /// [`ToolSpec::output_trust`] defaults to untrusted.
    #[serde(default = "Reach::external")]
    pub reach: Reach,
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
    /// What *this* result is, in [trust](crate::provenance) terms.
    ///
    /// The registry takes the lower of this and [`ToolSpec::output_trust`], so a tool can
    /// only ever narrow what its specification declared for one particular call — a memory
    /// store that returns a record written while the conversation was already tainted, a
    /// filesystem tool that read from a root the deployment did not vouch for. Raising trust
    /// is not expressible, which is the property that makes the declaration in the
    /// specification the ceiling rather than a hint.
    ///
    /// Deserialising defaults to [`Trust::Untrusted`]: a result that crossed a wire has been
    /// out of this process's hands, and the constructors below are how in-process code says
    /// otherwise.
    #[serde(default = "Trust::untrusted")]
    pub trust: Trust,
}

impl ToolOutcome {
    /// A successful outcome, as trusted as its tool's specification says.
    pub fn ok(output: impl Into<Value>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            trust: Trust::Trusted,
        }
    }

    /// A failure the model should see.
    ///
    /// Trusted by default like [`ToolOutcome::ok`], because a refusal is written by the tool
    /// itself. A tool that puts a third party's error text in the output should say so with
    /// [`ToolOutcome::with_trust`].
    pub fn error(output: impl Into<Value>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
            trust: Trust::Trusted,
        }
    }

    /// Narrows this result's trust.
    ///
    /// Only ever narrows: the registry combines this with the tool's declared
    /// [`ToolSpec::output_trust`] by [`Trust::min_with`], so passing [`Trust::Trusted`] to a
    /// tool whose specification says otherwise changes nothing.
    #[must_use]
    pub fn with_trust(mut self, trust: Trust) -> Self {
        self.trust = trust;
        self
    }
}

/// Something an agent or a model can invoke.
///
/// A `Tool` is never given directly to anything that isn't already trusted to run it —
/// see the [module-level security boundary](self#the-security-boundary). It is registered
/// with a [`ToolRegistry`] by the trusted code that constructs it, and from that point on
/// is only ever reached through [`ToolRegistry::invoke`].
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Describes the tool.
    fn spec(&self) -> ToolSpec;

    /// Declares the specific resources this call will act on, derived from its arguments.
    ///
    /// The registry authorizes every claim returned here *before* [`Tool::invoke`] runs, so
    /// a refused resource means the tool never executes at all. Returning an empty list —
    /// the default — means the tool is authorized at capability level only.
    ///
    /// The tool derives the claims because only the tool understands its own arguments: a
    /// filesystem tool knows which field is a path, and knows it must be resolved before
    /// it means anything. The registry deliberately does not try to extract resources from
    /// arbitrary JSON, because doing so safely requires exactly the domain knowledge it
    /// does not have.
    ///
    /// # Obligations
    ///
    /// Implementations must return the resource they will *actually* act on, in whatever
    /// canonical form the policy is written against — for a path, that means resolved,
    /// with `..` and symlinks eliminated, never the raw argument. Declaring
    /// `project/../../../etc/shadow` verbatim and letting the policy pattern-match on it is
    /// how path-traversal bugs happen.
    ///
    /// This method may touch the outside world in order to canonicalise (resolving a path
    /// requires consulting the filesystem). It must not perform the operation itself, and
    /// it must not have side effects: it can be called for a call that is then refused.
    ///
    /// Anything discovered *after* this point — a symlink that moved, entries found while
    /// walking a directory, a redirect — must be authorized through the
    /// [`ResourceAuthorizer`] handed to [`Tool::invoke`]. See
    /// [Time-of-check to time-of-use](self#time-of-check-to-time-of-use).
    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let _ = arguments;
        Ok(Vec::new())
    }

    /// Runs the tool.
    ///
    /// By the time this is called, [`ToolSpec::required_permissions`] and every claim from
    /// [`Tool::planned_resources`] have already been authorized by the registry — this
    /// method must not re-check them, and callers must never call it directly (there is no
    /// way to enforce that in Rust; it is a contract of this trait, upheld by never handing
    /// out a `dyn Tool`).
    ///
    /// `authorizer` is for resources that were *not* knowable in advance. A tool that only
    /// touches what it declared never needs it.
    ///
    /// Implementations must honour `cx`'s cancellation and deadline themselves — the same
    /// way [`ModelProvider`](crate::model::ModelProvider) implementations do — since only
    /// the implementation knows how to interrupt its own work. Nothing wraps this call
    /// with an external timeout: a wrapper that merely stops *waiting* without actually
    /// stopping the work would look like cancellation without being it.
    async fn invoke(
        &self,
        arguments: Value,
        authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome>;
}

impl std::fmt::Debug for dyn Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.spec().name)
            .finish()
    }
}

/// A source of tools to be aggregated into a [`ToolRegistry`].
///
/// This is a *supply-side* contract: something that contributes tools — a filesystem tool
/// set, an MCP server, a plugin's tools — implements it. It is not how an agent reaches a
/// tool; [`ToolCatalog::get`] returns a bare `Box<dyn Tool>`, which is only safe to call
/// from code that is itself trusted to feed a [`ToolRegistry`], never from anything that
/// might invoke it directly.
#[async_trait]
pub trait ToolCatalog: Send + Sync + 'static {
    /// Lists the tools this catalogue offers.
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ToolSpec>>;

    /// Fetches one tool by name.
    async fn get(&self, name: &ToolName, cx: &ExecutionContext) -> Result<Option<Box<dyn Tool>>>;
}

/// The single path through which a [`Tool`] may be invoked.
///
/// This is the enforcement half of the [security boundary](self#the-security-boundary):
/// an agent is only ever given a `dyn ToolRegistry`, never a `dyn Tool`, so there is
/// exactly one code path that can reach [`Tool::invoke`] — and that path always resolves
/// the tool's [`ToolSpec::required_permissions`] first. Implementations must guarantee
/// this; the `aik-tools` crate's `InProcessToolRegistry` is the reference implementation.
///
/// Registration is deliberately not part of this trait. Adding tools is a trusted,
/// setup-time operation performed by whatever assembles a registry (typically a kernel
/// [`Component`](aik_core::Component) during `init`); an agent downstream of that setup
/// only ever needs to discover and invoke, never to add.
#[async_trait]
pub trait ToolRegistry: Send + Sync + 'static {
    /// Lists every registered tool's specification.
    ///
    /// Listing is not authorized per tool: knowing a tool exists, and what its schema
    /// looks like, is not the same as being allowed to invoke it. Whether a particular
    /// invocation is granted is decided at [`ToolRegistry::invoke`] time, not here.
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ToolSpec>>;

    /// Authorizes and, if permitted, invokes a tool.
    ///
    /// Implementations must resolve, in order and all before [`Tool::invoke`] is called:
    ///
    /// 1. every permission in the tool's [`ToolSpec::required_permissions`], as a
    ///    capability-level question with no resource;
    /// 2. every [`ResourceClaim`] returned by [`Tool::planned_resources`].
    ///
    /// A tool that declares neither runs unconditionally — there is nothing to authorize.
    /// Any refusal at either stage produces
    /// [`aik_core::Error::PermissionDenied`] and the tool does not run: there is no
    /// partial, provisional or best-effort invocation. The tool is then handed a
    /// [`ResourceAuthorizer`] so it can ask about anything it discovers while running.
    ///
    /// Every decision, and the invocation itself, is published to the kernel event bus —
    /// see [`crate::audit`]. Failing to obtain a policy engine or an approval sink is a
    /// denial, never a pass-through.
    ///
    /// [`ApprovalSink`]: crate::permission::ApprovalSink
    async fn invoke(
        &self,
        name: &ToolName,
        arguments: Value,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome>;
}
