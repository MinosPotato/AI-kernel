//! Structured authorization and invocation events.
//!
//! Every authorization decision and every tool invocation is published on the kernel
//! [`EventBus`](aik_core::EventBus) as one of the events defined here. This reuses the
//! kernel's existing event mechanism rather than introducing a logging system of its own:
//! an audit sink is just another subscriber, and because every kernel event is
//! serialisable, an out-of-process auditor can consume them through the
//! [firehose](aik_core::EventBus::subscribe_any) without linking against this crate.
//!
//! The kernel itself still persists nothing. What this module adds beyond the events is the
//! *contract* a durable sink implements — [`AuditStore`], and the [`AuditRecord`] it keeps —
//! so that storage remains a subscriber's problem, in a subscriber's crate, exactly as
//! [`MemoryStore`](crate::memory::MemoryStore) and
//! [`ContextStore`](crate::context::ContextStore) are. `aik-audit` is the implementation;
//! nothing in the kernel or in this crate writes a byte.
//!
//! # What these events must never carry
//!
//! Audit records are the most widely copied, longest-lived data a system produces — they
//! get shipped to log aggregators, held for years, and read by people and tools that were
//! never in the original trust boundary. So they carry the *shape* of what happened, never
//! its contents:
//!
//! * **Tool arguments are never included.** A `filesystem.write` call's arguments contain
//!   the bytes being written; a hypothetical `http.request` call's arguments could contain
//!   an API token. Recording `action`, `resource` and the decision answers every audit
//!   question ("who was allowed to write where, and when") without any of that.
//! * **Tool output is never included**, for the same reason.
//! * [`Decision::Deny`](crate::permission::Decision::Deny) reasons *are* included, since
//!   they are authored by the policy engine — trusted, human-facing text — not by a model,
//!   a tool, or a user.
//!
//! A [`ResourceId`] is the one field that carries caller-influenced content, because an
//! audit trail that omits *what was touched* is not an audit trail. Policy authors should
//! treat resource identifiers as recorded in the clear.
//!
//! # Correlating decisions with execution
//!
//! Every event carries the [`CorrelationId`] of the
//! [`ExecutionContext`] it happened under, so the
//! authorization decisions and the invocation they gated join on one key. The kernel also
//! stamps the correlation id and the publishing component onto the event envelope; it is
//! repeated inside the payload so that an audit record remains self-contained if it is
//! ever separated from its envelope.

use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use aik_core::{Event, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::execution::ExecutionContext;
use crate::permission::{ActionId, Principal, PrincipalId, PrincipalKind, ResourceId};
use crate::tool::ToolName;

/// Which authorization stage a decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationPhase {
    /// "May this principal use this capability at all?" — resolved from
    /// [`ToolSpec::required_permissions`](crate::tool::ToolSpec::required_permissions)
    /// before the tool runs.
    Tool,
    /// "May this principal use it on *this* resource?" — resolved from
    /// [`Tool::planned_resources`](crate::tool::Tool::planned_resources) before the tool
    /// runs.
    Resource,
    /// The same question, asked by the tool mid-execution about a resource it only
    /// discovered while running.
    DiscoveredResource,
}

/// How an authorization question was answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AuthorizationOutcome {
    /// The policy engine allowed it outright.
    Allowed,
    /// The policy engine refused.
    Denied {
        /// The policy engine's stated reason.
        reason: String,
    },
    /// The policy engine deferred to a human, who approved.
    ApprovalGranted,
    /// The policy engine deferred to a human, who refused.
    ApprovalRefused,
    /// The policy engine deferred to a human, but no answer could be obtained.
    ///
    /// Covers both "no approval sink is configured" and a sink that failed to produce an
    /// answer — nobody attached to ask, nobody answered before the deadline, the frontend
    /// went away. Treated as a denial. Recorded distinctly because it means the deployment
    /// is misconfigured or unattended, not that anyone actually said no.
    ApprovalUnavailable,
    /// No policy decision could be obtained, so nothing could be allowed.
    ///
    /// Covers both "no policy engine is configured" and an engine that failed to evaluate.
    /// Also a denial, also a broken mechanism rather than a decision.
    PolicyUnavailable,
}

impl AuthorizationOutcome {
    /// True only when the operation was permitted.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed | Self::ApprovalGranted)
    }
}

/// One authorization question and its answer.
///
/// Published once per question — so a tool requiring two permissions and touching three
/// resources produces five of these before it executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationDecided {
    /// The operation this decision belongs to.
    pub correlation: CorrelationId,
    /// When the decision was made, by the kernel clock.
    pub timestamp: Timestamp,
    /// The tool the decision was made for.
    pub tool: ToolName,
    /// Who was asking.
    pub principal: PrincipalId,
    /// What kind of actor they are.
    pub principal_kind: PrincipalKind,
    /// Who they were acting for, if anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
    /// The capability in question.
    pub action: ActionId,
    /// The specific resource, for resource-level decisions.
    ///
    /// `None` for [`AuthorizationPhase::Tool`], where the question is about the capability
    /// itself rather than any particular target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceId>,
    /// Which stage asked.
    pub phase: AuthorizationPhase,
    /// How long this one decision took to reach, in milliseconds.
    ///
    /// Measured from the moment the question was formed to the moment
    /// [`AuthorizationOutcome`] was decided. Includes both policy evaluation and, when the
    /// policy asked for one, the approval wait — see [`AuthorizationDecided::approval_wait_ms`]
    /// to isolate the latter. A locally measured wall-clock duration (`std::time::Instant`),
    /// not a provider- or policy-engine-reported figure.
    #[serde(default)]
    pub duration_ms: u64,
    /// How long of `duration_ms` was spent specifically waiting on
    /// [`ApprovalSink::request_approval`](crate::permission::ApprovalSink::request_approval),
    /// in milliseconds.
    ///
    /// `Some` for [`AuthorizationOutcome::ApprovalGranted`]/
    /// [`AuthorizationOutcome::ApprovalRefused`], and for [`AuthorizationOutcome::ApprovalUnavailable`]
    /// when it resulted from a sink that was asked and failed. `None` for
    /// [`AuthorizationOutcome::Allowed`]/[`AuthorizationOutcome::Denied`]/
    /// [`AuthorizationOutcome::PolicyUnavailable`], and for the [`AuthorizationOutcome::ApprovalUnavailable`]
    /// case where no sink was configured to ask in the first place — in both, no approval
    /// wait happened at all.
    ///
    /// Broken out from `duration_ms` because the two have wildly different distributions: a
    /// policy check is sub-millisecond and in-memory, an approval wait is a human being asked
    /// a question and can run to minutes. Folding them into one number makes that number
    /// bimodal and useless for alerting on either half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_wait_ms: Option<u64>,
    /// The answer.
    #[serde(flatten)]
    pub outcome: AuthorizationOutcome,
}

impl Event for AuthorizationDecided {
    const NAME: &'static str = "aik.authorization.decided";
}

/// How an invocation ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum InvocationOutcome {
    /// The tool ran and reported success.
    Succeeded,
    /// The tool ran and reported a failure the caller was meant to see.
    ///
    /// This is [`ToolOutcome::is_error`](crate::tool::ToolOutcome::is_error) — the tool
    /// worked, the operation did not.
    ReportedError,
    /// The tool could not be run at all.
    Failed {
        /// The kernel error classification, e.g. `cancelled` or `timeout`.
        ///
        /// The classification rather than the message: an error string can embed
        /// caller-supplied content, a `kind` cannot.
        kind: String,
    },
    /// Authorization refused the call, so the tool never ran.
    Denied,
    /// No tool of that name is registered, so nothing ran.
    NotFound,
}

impl InvocationOutcome {
    /// True only when the tool ran and reported success.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// One completed (or refused) tool invocation.
///
/// Published exactly once per [`ToolRegistry::invoke`](crate::tool::ToolRegistry::invoke)
/// call, whatever the result — including calls that were denied or named a tool that does
/// not exist, both of which are worth seeing in an audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvoked {
    /// The operation this invocation belongs to.
    pub correlation: CorrelationId,
    /// When the invocation finished, by the kernel clock.
    pub timestamp: Timestamp,
    /// The tool that was asked for.
    ///
    /// Present even for [`InvocationOutcome::NotFound`], since a caller probing for tools
    /// that do not exist is itself worth recording.
    pub tool: ToolName,
    /// Who was asking.
    pub principal: PrincipalId,
    /// What kind of actor they are.
    pub principal_kind: PrincipalKind,
    /// Who they were acting for, if anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
    /// How long the whole call took, in milliseconds: authorization plus execution.
    ///
    /// Present even for [`InvocationOutcome::NotFound`] and
    /// [`InvocationOutcome::Denied`], where it measures whatever work happened before the
    /// call was refused. A locally measured wall-clock duration, not a provider figure.
    #[serde(default)]
    pub duration_ms: u64,
    /// How long authorization (every [`AuthorizationPhase::Tool`] and
    /// [`AuthorizationPhase::Resource`] question this call required) took, in milliseconds.
    ///
    /// `None` for [`InvocationOutcome::NotFound`], where no authorization question was ever
    /// asked. Includes any approval wait, for the same reason
    /// [`AuthorizationDecided::duration_ms`] does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_duration_ms: Option<u64>,
    /// How long the tool itself ran, in milliseconds, excluding authorization.
    ///
    /// `None` when the tool never ran at all — [`InvocationOutcome::NotFound`] or
    /// [`InvocationOutcome::Denied`]. This is the figure to read as "tool latency" in the
    /// narrow sense; [`ToolInvoked::duration_ms`] is the end-to-end figure most callers
    /// actually experience.
    ///
    /// Note this can still include authorization time: a tool such as
    /// `filesystem.list` asks [`AuthorizationPhase::DiscoveredResource`] questions *while
    /// it runs*, one per entry, which are structurally part of its execution rather than
    /// of the up-front [`ToolInvoked::authorization_duration_ms`] phase. Summing every
    /// [`AuthorizationDecided::duration_ms`] for the same correlation gives the true total
    /// authorization time when that distinction matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_duration_ms: Option<u64>,
    /// How it ended.
    #[serde(flatten)]
    pub outcome: InvocationOutcome,
}

impl Event for ToolInvoked {
    const NAME: &'static str = "aik.tool.invoked";
}

// ---------------------------------------------------------------------------------------
// The durable side: what a record is, how it is asked for, and who may see it.
// ---------------------------------------------------------------------------------------

/// Records that the audit trail lost events, and how many.
///
/// The event bus is a bounded broadcast: a subscriber that falls behind is told how many
/// messages it missed rather than being handed them late. For most subscribers that is a
/// nuisance; for an audit sink it is the single failure that matters, because a trail with a
/// silent hole in it is worse than no trail at all — it reads as a complete account of a
/// period in which nothing happened.
///
/// So the hole is written down. A sink that observes lag appends one of these before it
/// appends anything else, and [`AuditRecord::visible_to`] makes it visible in *every* view,
/// whoever is reading and whatever they filtered on. A reader can be denied another
/// principal's records; nobody can be denied the knowledge that records are missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditGap {
    /// When the loss was noticed, by the kernel clock.
    pub timestamp: Timestamp,
    /// How many events the bus reports were dropped before the sink caught up.
    pub missed: u64,
}

/// Records that retention removed part of the trail.
///
/// The counterpart of [`AuditGap`] for deliberate loss. An append-only log that can be
/// truncated is only honest if the truncation is itself in the log, so a sweep that removed
/// anything writes one of these, and it is visible in every view for the same reason a gap
/// is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionApplied {
    /// When the sweep ran, by the kernel clock.
    pub timestamp: Timestamp,
    /// Records at or before this instant were removed.
    pub cutoff: Timestamp,
    /// How many were removed.
    pub removed: u64,
}

/// What one audit record is about.
///
/// The two interesting variants hold the published event *verbatim* rather than a flattened
/// copy of its fields. A record that restated the principal, the tool and the outcome
/// alongside the event would be a record that can disagree with the event it claims to be,
/// and reconciling the two later is not a problem worth inventing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEntry {
    /// One authorization question and its answer.
    Authorization(AuthorizationDecided),
    /// One completed, refused or impossible tool invocation.
    Invocation(ToolInvoked),
    /// A hole in the trail: events the sink was told it missed.
    Gap(AuditGap),
    /// A deliberate hole: records retention removed.
    Retention(RetentionApplied),
}

/// Which sort of thing an [`AuditEntry`] is, without looking inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEntryKind {
    /// [`AuditEntry::Authorization`].
    Authorization,
    /// [`AuditEntry::Invocation`].
    Invocation,
    /// [`AuditEntry::Gap`].
    Gap,
    /// [`AuditEntry::Retention`].
    Retention,
}

impl AuditEntryKind {
    /// The kind's name, as a filter spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authorization => "authorization",
            Self::Invocation => "invocation",
            Self::Gap => "gap",
            Self::Retention => "retention",
        }
    }

    /// Whether this kind describes the trail itself rather than something a principal did.
    ///
    /// The two that do — a gap and a retention sweep — are the ones no filter may hide; see
    /// [`AuditRecord::visible_to`].
    pub fn is_about_the_trail(self) -> bool {
        matches!(self, Self::Gap | Self::Retention)
    }
}

impl AuditEntry {
    /// Which sort of entry this is.
    pub fn kind(&self) -> AuditEntryKind {
        match self {
            Self::Authorization(_) => AuditEntryKind::Authorization,
            Self::Invocation(_) => AuditEntryKind::Invocation,
            Self::Gap(_) => AuditEntryKind::Gap,
            Self::Retention(_) => AuditEntryKind::Retention,
        }
    }

    /// When it happened, by the kernel clock.
    pub fn timestamp(&self) -> Timestamp {
        match self {
            Self::Authorization(event) => event.timestamp,
            Self::Invocation(event) => event.timestamp,
            Self::Gap(gap) => gap.timestamp,
            Self::Retention(applied) => applied.timestamp,
        }
    }

    /// The operation it belongs to, for the two variants that belong to one.
    pub fn correlation(&self) -> Option<CorrelationId> {
        match self {
            Self::Authorization(event) => Some(event.correlation),
            Self::Invocation(event) => Some(event.correlation),
            Self::Gap(_) | Self::Retention(_) => None,
        }
    }

    /// Who acted.
    ///
    /// [`Principal::system`] for the two variants that describe the trail rather than an
    /// actor: they are the system's own bookkeeping, and giving them a real identity is what
    /// lets a store index every record the same way instead of carrying a special case into
    /// its tables.
    pub fn principal(&self) -> PrincipalId {
        match self {
            Self::Authorization(event) => event.principal.clone(),
            Self::Invocation(event) => event.principal.clone(),
            Self::Gap(_) | Self::Retention(_) => PrincipalId::new(Principal::SYSTEM),
        }
    }

    /// What kind of actor it was.
    pub fn principal_kind(&self) -> PrincipalKind {
        match self {
            Self::Authorization(event) => event.principal_kind,
            Self::Invocation(event) => event.principal_kind,
            Self::Gap(_) | Self::Retention(_) => PrincipalKind::System,
        }
    }

    /// Who they were acting for, if anyone.
    pub fn on_behalf_of(&self) -> Option<&PrincipalId> {
        match self {
            Self::Authorization(event) => event.on_behalf_of.as_ref(),
            Self::Invocation(event) => event.on_behalf_of.as_ref(),
            Self::Gap(_) | Self::Retention(_) => None,
        }
    }

    /// The tool involved, for the two variants that name one.
    pub fn tool(&self) -> Option<&ToolName> {
        match self {
            Self::Authorization(event) => Some(&event.tool),
            Self::Invocation(event) => Some(&event.tool),
            Self::Gap(_) | Self::Retention(_) => None,
        }
    }

    /// The resource, for a resource-level authorization decision.
    pub fn resource(&self) -> Option<&ResourceId> {
        match self {
            Self::Authorization(event) => event.resource.as_ref(),
            _ => None,
        }
    }

    /// Whether this entry records something being refused.
    ///
    /// True for any authorization that did not end in permission — a denial, a refused
    /// approval, an approval nobody could be asked for, a policy that could not be reached —
    /// and for an invocation that authorization stopped. It is deliberately *not* true for
    /// [`InvocationOutcome::Failed`] or [`InvocationOutcome::ReportedError`]: those are a
    /// tool going wrong, which is an operational question, whereas a refusal is an authority
    /// question, and an operator reviewing "what was this not allowed to do" should not have
    /// to read past broken plumbing to find it.
    pub fn is_refusal(&self) -> bool {
        match self {
            Self::Authorization(event) => !event.outcome.is_allowed(),
            Self::Invocation(event) => matches!(event.outcome, InvocationOutcome::Denied),
            Self::Gap(_) | Self::Retention(_) => false,
        }
    }
}

/// One entry, together with its position in the trail.
///
/// [`AuditRecord::sequence`] is assigned by the store on append and is the record's identity:
/// there is no separate id, because a gap in the numbering is exactly the thing an audit
/// reader wants to be able to see, and a random id would hide it.
///
/// # What the sequence orders, and what it does not
///
/// It is the order records were **written**, which is not quite the order events *happened*.
/// A sink that subscribes per event type receives each type in order but may interleave two
/// types published in the same instant either way round, so a decision and the invocation it
/// gated can land in either order when nothing separates them in time.
///
/// That is a property of the trail, not a defect to work around, and the record carries what
/// resolves it: [`AuditEntry::timestamp`] is the kernel clock's view of when the event
/// happened, and [`AuditEntry::correlation`] joins everything belonging to one operation.
/// Read the sequence as "position in the trail" — its job is to make a removal visible —
/// and the timestamp as "when".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Where this record sits in the trail. Starts at 1 and never repeats.
    pub sequence: u64,
    /// What happened.
    pub entry: AuditEntry,
}

impl AuditRecord {
    /// Whether `reader` is allowed to see this record.
    ///
    /// Three rules, in the order they are read:
    ///
    /// * A record *about the trail* — a [gap](AuditGap) or a
    ///   [retention sweep](RetentionApplied) — is visible to everyone. Whether the account is
    ///   complete is not a private fact about anyone, and a reader who cannot tell that
    ///   records are missing is being told a lie by omission.
    /// * A reader sees what they did, and what anything acting on their behalf did:
    ///   [`Principal::may_act_for`] against the record's actor, and the record's
    ///   `on_behalf_of` against the reader's own identity. The second half is what makes an
    ///   audit trail useful to the person an agent was delegated by — the whole point of
    ///   recording delegation.
    /// * Nothing else. A principal cannot read another's trail by naming them in a filter,
    ///   because filters narrow this rule and never widen it.
    ///
    /// The limit is the one stated for every other owned resource in the system: in-process
    /// code can construct a [`Principal`] naming anyone, so this is a boundary against a
    /// model — which can never construct one — and defence in depth against a confused
    /// caller, not a boundary against hostile code already inside the process. The audit
    /// trail additionally never reaches a model at all: no tool exposes it.
    pub fn visible_to(&self, reader: &Principal) -> bool {
        if self.entry.kind().is_about_the_trail() {
            return true;
        }
        reader.may_act_for(&self.entry.principal()) || self.entry.on_behalf_of() == Some(&reader.id)
    }

    /// Whether this record is one `principal` is a party to, as a filter means it.
    ///
    /// The same relation [`AuditRecord::visible_to`] uses, asked about an arbitrary
    /// principal rather than about the reader: the actor, or whoever the actor was acting
    /// for. A record about the trail matches any principal, so filtering by one cannot hide
    /// a gap.
    pub fn concerns(&self, principal: &PrincipalId) -> bool {
        if self.entry.kind().is_about_the_trail() {
            return true;
        }
        &self.entry.principal() == principal || self.entry.on_behalf_of() == Some(principal)
    }
}

/// Whether a flag is unset, so that a default one is left out of the serialised form.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Which records to return.
///
/// Every field narrows, and they combine conjunctively. A query that sets nothing but a limit
/// is "the most recent `limit` records I am allowed to see", which is what a person reviewing
/// a trail almost always wants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuery {
    /// Only records this principal is a party to — see [`AuditRecord::concerns`].
    ///
    /// Narrows what the reader may already see; it can never widen it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<PrincipalId>,
    /// Only records belonging to one operation, which is how a decision is joined to the
    /// invocation it gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<CorrelationId>,
    /// Only records naming this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolName>,
    /// Only these sorts of entry. Empty means every sort.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<AuditEntryKind>,
    /// Only records at or after this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    /// Only records at or before this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// Only records of something being refused — see [`AuditEntry::is_refusal`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub refusals_only: bool,
    /// At most this many records. `None` means every match, which on a long-lived trail is
    /// rarely what a caller wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl AuditQuery {
    /// Whether `record` satisfies every filter this query sets.
    ///
    /// Deliberately does *not* consider visibility: a store applies both, and keeping them
    /// apart is what stops a filter from being mistaken for a permission check. Shared by
    /// both implementations so that "matches" cannot come to mean two things.
    pub fn matches(&self, record: &AuditRecord) -> bool {
        if let Some(principal) = &self.principal
            && !record.concerns(principal)
        {
            return false;
        }
        if let Some(correlation) = &self.correlation
            && record.entry.correlation().as_ref() != Some(correlation)
        {
            return false;
        }
        if let Some(tool) = &self.tool
            && record.entry.tool() != Some(tool)
        {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&record.entry.kind()) {
            return false;
        }
        if let Some(since) = self.since
            && record.entry.timestamp() < since
        {
            return false;
        }
        if let Some(until) = self.until
            && record.entry.timestamp() > until
        {
            return false;
        }
        if self.refusals_only && !record.entry.is_refusal() {
            return false;
        }
        true
    }
}

/// An append-only home for the audit trail.
///
/// # Append-only means append-only
///
/// There is no update and no delete on this trait, and that absence is the contract rather
/// than an omission to be filled in later. A record, once written, is what happened; code
/// that could rewrite it could rewrite the account of its own authority. Reclaiming space is
/// a separate concern with a separate mechanism — a retention sweep, owner-blind, bounded by
/// a cutoff, and obliged to write a [`RetentionApplied`] record of what it removed — and it
/// lives in the implementation crate precisely so that holding an `Arc<dyn AuditStore>` does
/// not hand anyone the ability to erase.
///
/// # Why `append` takes no principal
///
/// Every other store in this crate authorizes against the calling
/// [`ExecutionContext`]. This one does not, because the
/// subject of an audit record is inside the entry: it is a copy of an event the
/// [`ToolRegistry`](crate::tool::ToolRegistry) published, and anything able to call `append`
/// with a forged entry is equally able to publish a forged event. Taking a principal here
/// would imply a check that cannot be made, which is worse than not taking one.
///
/// Reads *are* authorized, from `cx`, against [`AuditRecord::visible_to`].
///
/// # Obligations
///
/// * **Durable on return.** When [`AuditStore::append`] returns `Ok`, the record survives
///   losing the process.
/// * **Monotonic.** Sequence numbers start at 1, increase by one per record, and are never
///   reused — including across restarts. A reader may treat a break in the numbering as
///   evidence of tampering.
/// * **Exact on read.** A stored record that cannot be decoded is corruption and must be
///   reported, never skipped. An audit store that quietly dropped what it could not read
///   would be indistinguishable from one with nothing to show.
/// * **Filtering is not authorization.** [`AuditStore::query`] applies
///   [`AuditRecord::visible_to`] to every candidate, whatever the query asked for, and omits
///   what the reader may not see rather than erroring — an error would confirm the record
///   exists.
#[async_trait]
pub trait AuditStore: Send + Sync + 'static {
    /// Appends one entry, returning the sequence number it was given.
    async fn append(&self, entry: AuditEntry) -> Result<u64>;

    /// Returns matching records the caller may see, newest first.
    async fn query(&self, query: &AuditQuery, cx: &ExecutionContext) -> Result<Vec<AuditRecord>>;

    /// The highest sequence number ever issued, or zero for a trail nothing has been
    /// appended to.
    ///
    /// Counts what was *written*, not what is still there: retention removes records and
    /// never renumbers the rest, so this does not decrease when a sweep runs. That is what
    /// makes it useful — a reader holding an exported trail can tell how many records the
    /// store has issued in total, and therefore whether the export is short.
    ///
    /// Unauthorized on purpose, and safe to be: it reveals how many records exist and nothing
    /// whatever about them.
    async fn last_sequence(&self) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decision() -> AuthorizationDecided {
        AuthorizationDecided {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1_000),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            action: ActionId::new("demo.act"),
            resource: Some(ResourceId::new("/tmp/x")),
            phase: AuthorizationPhase::Resource,
            duration_ms: 5,
            approval_wait_ms: None,
            outcome: AuthorizationOutcome::Allowed,
        }
    }

    #[test]
    fn authorization_events_round_trip() {
        let event = decision();
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["outcome"], json!("allowed"));
        assert_eq!(json["phase"], json!("resource"));
        assert_eq!(json["resource"], json!("/tmp/x"));
        assert!(json.get("approval_wait_ms").is_none());

        let parsed: AuthorizationDecided = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn an_approval_decision_reports_the_wait_separately_from_the_total() {
        let event = AuthorizationDecided {
            duration_ms: 4_012,
            approval_wait_ms: Some(4_009),
            outcome: AuthorizationOutcome::ApprovalGranted,
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["duration_ms"], json!(4_012));
        assert_eq!(json["approval_wait_ms"], json!(4_009));

        let parsed: AuthorizationDecided = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn denials_carry_the_policy_authored_reason() {
        let event = AuthorizationDecided {
            outcome: AuthorizationOutcome::Denied {
                reason: "outside the workspace".into(),
            },
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["outcome"], json!("denied"));
        assert_eq!(json["reason"], json!("outside the workspace"));
    }

    #[test]
    fn tool_level_decisions_omit_the_resource() {
        let event = AuthorizationDecided {
            resource: None,
            phase: AuthorizationPhase::Tool,
            ..decision()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("resource").is_none());
    }

    #[test]
    fn invocation_events_round_trip() {
        let event = ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(2_000),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: Some(PrincipalId::new("user-1")),
            duration_ms: 12,
            authorization_duration_ms: Some(3),
            execution_duration_ms: Some(9),
            outcome: InvocationOutcome::Failed {
                kind: "timeout".into(),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["result"], json!("failed"));
        assert_eq!(json["kind"], json!("timeout"));
        assert_eq!(json["on_behalf_of"], json!("user-1"));
        assert_eq!(json["duration_ms"], json!(12));
        assert_eq!(json["authorization_duration_ms"], json!(3));
        assert_eq!(json["execution_duration_ms"], json!(9));

        let parsed: ToolInvoked = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn a_tool_invocation_that_never_ran_omits_its_execution_duration() {
        let event = ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(2_000),
            tool: ToolName::new("ghost"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            duration_ms: 1,
            authorization_duration_ms: None,
            execution_duration_ms: None,
            outcome: InvocationOutcome::NotFound,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("authorization_duration_ms").is_none());
        assert!(json.get("execution_duration_ms").is_none());
    }

    #[test]
    fn outcomes_classify_themselves() {
        assert!(AuthorizationOutcome::Allowed.is_allowed());
        assert!(AuthorizationOutcome::ApprovalGranted.is_allowed());
        assert!(!AuthorizationOutcome::ApprovalRefused.is_allowed());
        assert!(!AuthorizationOutcome::ApprovalUnavailable.is_allowed());
        assert!(!AuthorizationOutcome::PolicyUnavailable.is_allowed());
        assert!(!AuthorizationOutcome::Denied { reason: "n".into() }.is_allowed());

        assert!(InvocationOutcome::Succeeded.is_success());
        assert!(!InvocationOutcome::ReportedError.is_success());
        assert!(!InvocationOutcome::Denied.is_success());
    }

    fn invocation() -> ToolInvoked {
        ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(2_000),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent-1"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: Some(PrincipalId::new("user-1")),
            duration_ms: 12,
            authorization_duration_ms: Some(3),
            execution_duration_ms: Some(9),
            outcome: InvocationOutcome::Succeeded,
        }
    }

    fn record(entry: AuditEntry) -> AuditRecord {
        AuditRecord { sequence: 1, entry }
    }

    #[test]
    fn every_entry_variant_round_trips() {
        for entry in [
            AuditEntry::Authorization(decision()),
            AuditEntry::Invocation(invocation()),
            AuditEntry::Gap(AuditGap {
                timestamp: Timestamp::from_millis(3),
                missed: 7,
            }),
            AuditEntry::Retention(RetentionApplied {
                timestamp: Timestamp::from_millis(9),
                cutoff: Timestamp::from_millis(4),
                removed: 11,
            }),
        ] {
            let stored = record(entry.clone());
            let json = serde_json::to_value(&stored).unwrap();
            let parsed: AuditRecord = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, stored);
            assert_eq!(parsed.entry.kind(), entry.kind());
        }
    }

    #[test]
    fn an_entry_reports_what_the_event_inside_it_says() {
        let event = decision();
        let entry = AuditEntry::Authorization(event.clone());
        assert_eq!(entry.timestamp(), event.timestamp);
        assert_eq!(entry.correlation(), Some(event.correlation));
        assert_eq!(entry.principal(), event.principal);
        assert_eq!(entry.principal_kind(), event.principal_kind);
        assert_eq!(entry.tool(), Some(&event.tool));
        assert_eq!(entry.resource(), event.resource.as_ref());
    }

    #[test]
    fn an_entry_about_the_trail_is_attributed_to_the_system_and_names_no_tool() {
        let entry = AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(3),
            missed: 2,
        });
        assert_eq!(entry.principal(), PrincipalId::new(Principal::SYSTEM));
        assert_eq!(entry.principal_kind(), PrincipalKind::System);
        assert_eq!(entry.tool(), None);
        assert_eq!(entry.correlation(), None);
        assert!(entry.kind().is_about_the_trail());
    }

    #[test]
    fn a_refusal_is_an_authority_failure_not_an_operational_one() {
        let refused = |outcome| {
            AuditEntry::Authorization(AuthorizationDecided {
                outcome,
                ..decision()
            })
            .is_refusal()
        };
        assert!(!refused(AuthorizationOutcome::Allowed));
        assert!(!refused(AuthorizationOutcome::ApprovalGranted));
        assert!(refused(AuthorizationOutcome::Denied {
            reason: "no".into()
        }));
        assert!(refused(AuthorizationOutcome::ApprovalRefused));
        assert!(refused(AuthorizationOutcome::ApprovalUnavailable));
        assert!(refused(AuthorizationOutcome::PolicyUnavailable));

        let ended = |outcome| {
            AuditEntry::Invocation(ToolInvoked {
                outcome,
                ..invocation()
            })
            .is_refusal()
        };
        assert!(ended(InvocationOutcome::Denied));
        assert!(!ended(InvocationOutcome::Succeeded));
        // A tool that broke is an operational problem, not an authority one: an operator
        // asking "what was refused" must not have to read past it.
        assert!(!ended(InvocationOutcome::Failed {
            kind: "timeout".into()
        }));
        assert!(!ended(InvocationOutcome::ReportedError));
        assert!(!ended(InvocationOutcome::NotFound));
    }

    #[test]
    fn a_reader_sees_their_own_records_and_what_was_done_on_their_behalf() {
        let stored = record(AuditEntry::Invocation(invocation()));

        let agent = Principal::new("agent-1", PrincipalKind::Agent);
        let user = Principal::new("user-1", PrincipalKind::User);
        assert!(stored.visible_to(&agent), "the actor sees what it did");
        assert!(
            stored.visible_to(&user),
            "the principal an agent acted for sees what was done for them"
        );
    }

    #[test]
    fn a_reader_sees_nothing_of_a_principal_they_have_no_relation_to() {
        let stored = record(AuditEntry::Invocation(invocation()));
        let stranger = Principal::new("mallory", PrincipalKind::User);
        assert!(!stored.visible_to(&stranger));

        // Nor does naming the principal in a filter change that: `matches` and `visible_to`
        // are separate, and a store applies both.
        let query = AuditQuery {
            principal: Some(PrincipalId::new("agent-1")),
            ..AuditQuery::default()
        };
        assert!(query.matches(&stored));
        assert!(!stored.visible_to(&stranger));
    }

    #[test]
    fn the_system_principal_is_an_identity_here_too_not_a_master_key() {
        let stored = record(AuditEntry::Invocation(invocation()));
        assert!(!stored.visible_to(&Principal::system()));
    }

    #[test]
    fn a_gap_is_visible_to_everyone_and_matches_every_principal_filter() {
        let stored = record(AuditEntry::Gap(AuditGap {
            timestamp: Timestamp::from_millis(3),
            missed: 4,
        }));
        for reader in [
            Principal::new("mallory", PrincipalKind::User),
            Principal::new("agent-1", PrincipalKind::Agent),
            Principal::system(),
        ] {
            assert!(
                stored.visible_to(&reader),
                "nobody may be told a truncated trail is a complete one"
            );
        }

        let query = AuditQuery {
            principal: Some(PrincipalId::new("someone-else")),
            ..AuditQuery::default()
        };
        assert!(query.matches(&stored), "a filter must not hide a gap");
    }

    #[test]
    fn a_retention_marker_is_visible_on_the_same_terms_as_a_gap() {
        let stored = record(AuditEntry::Retention(RetentionApplied {
            timestamp: Timestamp::from_millis(9),
            cutoff: Timestamp::from_millis(4),
            removed: 3,
        }));
        assert!(stored.visible_to(&Principal::new("mallory", PrincipalKind::User)));
    }

    #[test]
    fn each_query_filter_narrows_on_its_own() {
        let event = invocation();
        let stored = record(AuditEntry::Invocation(event.clone()));

        assert!(AuditQuery::default().matches(&stored));

        let wrong_principal = AuditQuery {
            principal: Some(PrincipalId::new("nobody")),
            ..AuditQuery::default()
        };
        assert!(!wrong_principal.matches(&stored));

        let right_delegator = AuditQuery {
            principal: Some(PrincipalId::new("user-1")),
            ..AuditQuery::default()
        };
        assert!(right_delegator.matches(&stored));

        let wrong_correlation = AuditQuery {
            correlation: Some(CorrelationId::new()),
            ..AuditQuery::default()
        };
        assert!(!wrong_correlation.matches(&stored));
        let right_correlation = AuditQuery {
            correlation: Some(event.correlation),
            ..AuditQuery::default()
        };
        assert!(right_correlation.matches(&stored));

        let wrong_tool = AuditQuery {
            tool: Some(ToolName::new("other.tool")),
            ..AuditQuery::default()
        };
        assert!(!wrong_tool.matches(&stored));

        let wrong_kind = AuditQuery {
            kinds: vec![AuditEntryKind::Authorization],
            ..AuditQuery::default()
        };
        assert!(!wrong_kind.matches(&stored));

        let too_late = AuditQuery {
            since: Some(Timestamp::from_millis(2_001)),
            ..AuditQuery::default()
        };
        assert!(!too_late.matches(&stored));
        let too_early = AuditQuery {
            until: Some(Timestamp::from_millis(1_999)),
            ..AuditQuery::default()
        };
        assert!(!too_early.matches(&stored));

        // The bounds are inclusive at both ends, so a record exactly on one is in range.
        let exactly = AuditQuery {
            since: Some(event.timestamp),
            until: Some(event.timestamp),
            ..AuditQuery::default()
        };
        assert!(exactly.matches(&stored));

        let refusals = AuditQuery {
            refusals_only: true,
            ..AuditQuery::default()
        };
        assert!(!refusals.matches(&stored));
    }

    #[test]
    fn a_query_serialises_without_the_filters_it_does_not_set() {
        let json = serde_json::to_value(AuditQuery::default()).unwrap();
        assert_eq!(json, json!({}));

        let parsed: AuditQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!(parsed, AuditQuery::default());
    }

    #[test]
    fn every_entry_kind_has_a_name_and_only_the_trail_kinds_are_undeniable() {
        for (kind, name) in [
            (AuditEntryKind::Authorization, "authorization"),
            (AuditEntryKind::Invocation, "invocation"),
            (AuditEntryKind::Gap, "gap"),
            (AuditEntryKind::Retention, "retention"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(
                kind.is_about_the_trail(),
                matches!(kind, AuditEntryKind::Gap | AuditEntryKind::Retention)
            );
        }
    }
}
