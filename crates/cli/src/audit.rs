//! `aik audit`: reading the durable trail, and pruning it.
//!
//! This is the operator's window onto [`aik_audit`]. It starts no model, no agent and no
//! tools: it resolves the database path the same way a run does, opens the trail, asks one
//! question and prints the answer.
//!
//! # Who the review runs as
//!
//! A run is `Principal::new(agent, Agent).on_behalf_of(user)` — the model acting for the
//! person. A review is the *person*, `Principal::new(user, User)`, and the difference
//! matters:
//!
//! * It is the truthful attribution. Nobody is delegating anything to a model here; a human
//!   typed `aik audit` at a terminal.
//! * It is what the trail is for. The visibility rule shows a reader what they did and what
//!   was done on their behalf, so reading as the user shows the whole of what that user's
//!   agents did for them — which is exactly the question somebody reviewing an agent has.
//!
//! # Why there is no tool for this
//!
//! Every other durable subsystem is reachable by an agent through the tool registry, so that
//! policy is consulted. The audit trail is the record of those consultations. A model that
//! could read it would be reading a map of where the boundaries are and where somebody has
//! already been refused; one that could prune it could edit the account of its own behaviour.
//! So the only path is this one: a human, at a terminal, against a file mode `0600` in a
//! directory mode `0700`.
//!
//! # Two ways to the same trail
//!
//! redb locks the database, so while a host process is running there is no second process
//! that can open it. `--socket` therefore asks the host instead, and the host answers the
//! same question under the same reading identity — the operator, not the agent. Nothing about
//! visibility changes: the store applies
//! [`AuditRecord::visible_to`](aik_api::audit::AuditRecord::visible_to) either way, and a
//! socket in a `0700` directory establishes exactly what opening a `0600` file establishes,
//! which is that the caller is the account that owns the database.
//!
//! The one thing that would be different — a *model* reaching this — is different in neither
//! direction. There is no audit tool, and the protocol carries no way to reach the registry.

use std::sync::Arc;

use aik_api::audit::{
    AuditEntry, AuditQuery, AuditRecord, AuditStore, AuthorizationOutcome, InvocationOutcome,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_api::tool::ToolName;
use aik_audit::{AuditRetentionSweeper, RedbAuditStore};
use aik_core::clock::Timestamp;
use aik_core::{Error, Result};
use aik_store::Db;

use aik_ipc::protocol::{Reply, Request};

use crate::args::{
    AuditCommand, AuditFilters, AuditOptions, DEFAULT_AUDIT_LIMIT, Options, PROGRAM,
};
use crate::settings::Settings;

/// Runs `aik audit`.
pub async fn run(options: &AuditOptions) -> Result<()> {
    let settings = resolve(options)?;
    let trail = Trail::open(&settings)?;

    match &options.command {
        AuditCommand::Review(filters) => review(&trail, filters, &settings.runtime.user).await,
        AuditCommand::Prune {
            older_than,
            dry_run,
        } => prune(&trail, *older_than, *dry_run).await,
    }
}

/// Where the records are read from.
///
/// Two sources, one set of questions. Which one is in use is decided once, here, so that
/// everything below — the query, the visibility rule, the rendering, the warning about a
/// short trail — is the same code whichever answered.
enum Trail {
    /// The database file, opened directly. Only possible when no host holds it.
    File(Arc<RedbAuditStore>),
    /// A running host process, asked over its socket.
    Host(std::path::PathBuf),
}

impl Trail {
    /// Decides which source these options name, and prepares it.
    fn open(settings: &Settings) -> Result<Self> {
        if let Some(socket) = &settings.socket {
            return Ok(Self::Host(socket.clone()));
        }
        let path = settings.database().ok_or_else(|| {
            Error::other(
                "there is no database to review; audit records are only kept when one is \
                 configured",
            )
        })?;
        let db = Arc::new(Db::open(path).map_err(|error| {
            Error::wrap(
                "opening the audit trail. If a host process is running it holds the database \
                 open; ask it instead with `--socket <PATH>`",
                error,
            )
        })?);
        Ok(Self::File(Arc::new(RedbAuditStore::new(db)?)))
    }

    /// The matching records, how many the trail has ever issued, and who it was read as.
    ///
    /// The reading identity is returned rather than assumed, because the two sources decide it
    /// differently and only one of them is this command's to decide. Against the file it is
    /// the person who typed the command; through a host it is the identity that host was
    /// configured with, and reporting the local one would name an identity that had nothing to
    /// do with what came back.
    async fn query(
        &self,
        query: &AuditQuery,
        user: &PrincipalId,
    ) -> Result<(Vec<AuditRecord>, u64, PrincipalId)> {
        match self {
            Self::File(store) => {
                // The reader is the *person*: `Principal::new(user, User)`, never the agent.
                // See this module's own documentation for why.
                let reader = Principal::new(user.clone(), PrincipalKind::User);
                let cx = ExecutionContext::new().with_principal(reader);
                let records = store.query(query, &cx).await?;
                let issued = store.last_sequence().await?;
                Ok((records, issued, user.clone()))
            }
            Self::Host(socket) => {
                let (reply, reader) = crate::client::audit(
                    socket,
                    Request::Audit {
                        query: query.clone(),
                    },
                )
                .await?;
                match reply {
                    Reply::Audit { records, issued } => Ok((records, issued, reader)),
                    other => aik_ipc::protocol::unexpected("audit records", &other),
                }
            }
        }
    }

    /// Removes, or counts, records at or before `cutoff`.
    async fn prune(&self, cutoff: Timestamp, dry_run: bool) -> Result<u64> {
        match self {
            Self::File(store) => {
                let count = if dry_run {
                    store.count_older_than(cutoff).await?
                } else {
                    store.sweep_older_than(cutoff).await?
                };
                Ok(count as u64)
            }
            Self::Host(socket) => {
                let older_than_ms = Timestamp::now()
                    .as_millis()
                    .saturating_sub(cutoff.as_millis());
                let (reply, _) = crate::client::audit(
                    socket,
                    Request::Prune {
                        older_than_ms,
                        dry_run,
                    },
                )
                .await?;
                match reply {
                    Reply::Pruned { removed, .. } => Ok(removed),
                    other => aik_ipc::protocol::unexpected("a prune result", &other),
                }
            }
        }
    }
}

/// Resolves the database path and the reading identity, reusing a run's own resolution.
///
/// Deliberately the same code path a run takes — the same precedence between `--db`, the
/// configuration file and the XDG default — so that `aik audit` cannot end up reading a
/// different database from the one `aik` writes to.
fn resolve(options: &AuditOptions) -> Result<Settings> {
    Settings::resolve(&Options {
        database: options.database.clone(),
        config: options.config.clone(),
        user: options.user.clone(),
        socket: options.socket.clone(),
        ..Options::default()
    })
}

/// Prints matching records, newest first.
async fn review(trail: &Trail, filters: &AuditFilters, user: &PrincipalId) -> Result<()> {
    let query = build_query(filters, Timestamp::now());
    let (records, total, user) = trail.query(&query, user).await?;

    if filters.json {
        for record in &records {
            println!(
                "{}",
                serde_json::to_string(record)
                    .map_err(|error| Error::wrap("rendering an audit record as JSON", error))?
            );
        }
        return Ok(());
    }

    if records.is_empty() {
        println!("no audit records match.");
    }
    for record in &records {
        println!("{}", line(record));
    }

    let gaps = records
        .iter()
        .filter(|record| record.entry.kind().is_about_the_trail())
        .count();
    // `total` is how many the store has ever issued, which is not how many it still holds:
    // retention removes records and never renumbers the rest. Printed so a reader can see at
    // a glance that the window they asked for is a window.
    println!(
        "\n{} record(s) shown as `{user}`; {total} issued in total.",
        records.len()
    );
    if gaps > 0 {
        // Said out loud rather than left to be spotted in the list: a trail with a hole in it
        // is the one thing a reader must not scroll past.
        println!(
            "{gaps} of them say the trail is incomplete — see the `gap` and `retention` lines."
        );
    }
    Ok(())
}

/// Removes, or counts, records older than `older_than`.
async fn prune(trail: &Trail, older_than: std::time::Duration, dry_run: bool) -> Result<()> {
    let cutoff = cutoff(Timestamp::now(), older_than);

    if dry_run {
        let due = trail.prune(cutoff, true).await?;
        println!(
            "{due} record(s) are older than the given period and would be removed.\n\
             Nothing was removed; drop --dry-run to remove them."
        );
        return Ok(());
    }

    let removed = trail.prune(cutoff, false).await?;
    match removed {
        0 => println!("nothing is old enough to remove."),
        removed => println!(
            "{removed} record(s) removed. The trail records that this happened; run \
             `{PROGRAM} audit --kind retention` to see it."
        ),
    }
    Ok(())
}

/// The instant `older_than` before `now`, saturating at the epoch.
///
/// Saturating for the same reason the background sweep's cutoff is: a period longer than the
/// clock has run must select nothing, not wrap past every record and select the lot.
fn cutoff(now: Timestamp, older_than: std::time::Duration) -> Timestamp {
    let millis = u64::try_from(older_than.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(now.as_millis().saturating_sub(millis))
}

/// Turns command-line filters into a store query.
fn build_query(filters: &AuditFilters, now: Timestamp) -> AuditQuery {
    AuditQuery {
        principal: filters.principal.as_deref().map(PrincipalId::new),
        correlation: filters.correlation,
        tool: filters.tool.as_deref().map(ToolName::new),
        kinds: filters.kind.into_iter().collect(),
        since: filters.since.map(|since| cutoff(now, since)),
        until: None,
        refusals_only: filters.refusals,
        limit: Some(filters.limit.unwrap_or(DEFAULT_AUDIT_LIMIT)),
    }
}

/// One record, as a line.
fn line(record: &AuditRecord) -> String {
    let when = format_timestamp(record.entry.timestamp());
    let who = actor(record);
    match &record.entry {
        AuditEntry::Authorization(event) => format!(
            "#{seq:<6} {when}  authorization  {who}  {tool}  {outcome}{resource}",
            seq = record.sequence,
            tool = event.tool,
            outcome = authorization_outcome(&event.outcome),
            resource = event
                .resource
                .as_ref()
                .map(|resource| format!("  {resource}"))
                .unwrap_or_default(),
        ),
        AuditEntry::Invocation(event) => format!(
            "#{seq:<6} {when}  invocation     {who}  {tool}  {outcome}  {duration}ms",
            seq = record.sequence,
            tool = event.tool,
            outcome = invocation_outcome(&event.outcome),
            duration = event.duration_ms,
        ),
        AuditEntry::Gap(gap) => format!(
            "#{seq:<6} {when}  gap            *** {missed} event(s) were dropped and are not \
             in this trail ***",
            seq = record.sequence,
            missed = gap.missed,
        ),
        AuditEntry::Retention(applied) => format!(
            "#{seq:<6} {when}  retention      *** {removed} record(s) at or before {cutoff} \
             were removed ***",
            seq = record.sequence,
            removed = applied.removed,
            cutoff = format_timestamp(applied.cutoff),
        ),
    }
}

/// Who acted, and for whom.
fn actor(record: &AuditRecord) -> String {
    match record.entry.on_behalf_of() {
        Some(delegator) => format!("{} for {delegator}", record.entry.principal()),
        None => record.entry.principal().to_string(),
    }
}

/// How an authorization ended, in one word where possible.
fn authorization_outcome(outcome: &AuthorizationOutcome) -> String {
    match outcome {
        AuthorizationOutcome::Allowed => "allowed".into(),
        AuthorizationOutcome::Denied { reason } => format!("DENIED ({reason})"),
        AuthorizationOutcome::ApprovalGranted => "approved".into(),
        AuthorizationOutcome::ApprovalRefused => "REFUSED".into(),
        AuthorizationOutcome::ApprovalUnavailable => "DENIED (nobody could be asked)".into(),
        AuthorizationOutcome::PolicyUnavailable => "DENIED (no policy)".into(),
    }
}

/// How an invocation ended.
fn invocation_outcome(outcome: &InvocationOutcome) -> String {
    match outcome {
        InvocationOutcome::Succeeded => "ok".into(),
        InvocationOutcome::ReportedError => "tool error".into(),
        InvocationOutcome::Failed { kind } => format!("FAILED ({kind})"),
        InvocationOutcome::Denied => "DENIED".into(),
        InvocationOutcome::NotFound => "no such tool".into(),
    }
}

/// Formats a kernel timestamp as UTC, to the second: `2025-08-22T17:19:04Z`.
///
/// Written here rather than taken from a dependency because it is the only date this
/// workspace ever formats, and the alternative is a calendar library — with its own data
/// tables and its own release cadence — inside a security-sensitive binary, to print
/// twenty characters.
///
/// The civil-date conversion is Howard Hinnant's `civil_from_days`, which is exact for every
/// date this can produce: a [`Timestamp`] is milliseconds since the Unix epoch and cannot be
/// negative, so there is no pre-1970 case to get wrong.
pub fn format_timestamp(timestamp: Timestamp) -> String {
    let seconds = timestamp.as_millis() / 1_000;
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z",
        hour = time / 3_600,
        minute = (time % 3_600) / 60,
        second = time % 60,
    )
}

/// The civil date `days` days after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::audit::{
        AuditEntryKind, AuditGap, AuthorizationDecided, AuthorizationPhase, RetentionApplied,
    };
    use aik_api::permission::{ActionId, ResourceId};
    use aik_core::id::CorrelationId;

    fn decided(outcome: AuthorizationOutcome) -> AuditRecord {
        AuditRecord {
            sequence: 7,
            entry: AuditEntry::Authorization(AuthorizationDecided {
                correlation: CorrelationId::new(),
                timestamp: Timestamp::from_millis(1_755_882_000_000),
                tool: ToolName::new("filesystem.read"),
                principal: PrincipalId::new("assistant"),
                principal_kind: PrincipalKind::Agent,
                on_behalf_of: Some(PrincipalId::new("alice")),
                action: ActionId::new("fs.read"),
                resource: Some(ResourceId::new("/srv/notes.txt")),
                scope_trust: None,
                phase: AuthorizationPhase::Resource,
                duration_ms: 1,
                approval_wait_ms: None,
                outcome,
            }),
        }
    }

    #[test]
    fn the_epoch_formats_as_the_epoch() {
        assert_eq!(
            format_timestamp(Timestamp::from_millis(0)),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn a_known_instant_formats_exactly() {
        // Checked against `date -u -d @1755882000`.
        assert_eq!(
            format_timestamp(Timestamp::from_millis(1_755_882_000_000)),
            "2025-08-22T17:00:00Z"
        );
    }

    #[test]
    fn a_leap_day_formats_as_a_leap_day() {
        // 2024-02-29T00:00:00Z.
        assert_eq!(
            format_timestamp(Timestamp::from_millis(1_709_164_800_000)),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn a_line_names_the_actor_and_who_they_acted_for() {
        let rendered = line(&decided(AuthorizationOutcome::Allowed));
        assert!(rendered.contains("assistant for alice"), "{rendered}");
        assert!(rendered.contains("filesystem.read"), "{rendered}");
        assert!(rendered.contains("/srv/notes.txt"), "{rendered}");
        assert!(rendered.contains("allowed"), "{rendered}");
    }

    #[test]
    fn a_denial_carries_the_policys_reason() {
        let rendered = line(&decided(AuthorizationOutcome::Denied {
            reason: "outside the workspace".into(),
        }));
        assert!(rendered.contains("DENIED"), "{rendered}");
        assert!(rendered.contains("outside the workspace"), "{rendered}");
    }

    #[test]
    fn a_gap_says_what_it_is_in_words_rather_than_a_code() {
        let rendered = line(&AuditRecord {
            sequence: 9,
            entry: AuditEntry::Gap(AuditGap {
                timestamp: Timestamp::from_millis(0),
                missed: 12,
            }),
        });
        assert!(rendered.contains("12 event(s) were dropped"), "{rendered}");
    }

    #[test]
    fn a_retention_marker_says_what_went_and_from_when() {
        let rendered = line(&AuditRecord {
            sequence: 10,
            entry: AuditEntry::Retention(RetentionApplied {
                timestamp: Timestamp::from_millis(1_755_882_000_000),
                cutoff: Timestamp::from_millis(0),
                removed: 5,
            }),
        });
        assert!(rendered.contains("5 record(s)"), "{rendered}");
        assert!(rendered.contains("1970-01-01T00:00:00Z"), "{rendered}");
    }

    #[test]
    fn filters_become_the_query_they_describe() {
        let now = Timestamp::from_millis(1_000_000);
        let query = build_query(
            &AuditFilters {
                principal: Some("alice".into()),
                tool: Some("filesystem.read".into()),
                correlation: None,
                since: Some(std::time::Duration::from_secs(60)),
                kind: Some(AuditEntryKind::Invocation),
                refusals: true,
                limit: Some(5),
                json: false,
            },
            now,
        );

        assert_eq!(query.principal, Some(PrincipalId::new("alice")));
        assert_eq!(query.tool, Some(ToolName::new("filesystem.read")));
        assert_eq!(query.kinds, vec![AuditEntryKind::Invocation]);
        assert_eq!(query.since, Some(Timestamp::from_millis(940_000)));
        assert!(query.refusals_only);
        assert_eq!(query.limit, Some(5));
    }

    #[test]
    fn a_review_with_no_limit_still_has_one() {
        let query = build_query(&AuditFilters::default(), Timestamp::from_millis(0));
        assert_eq!(query.limit, Some(DEFAULT_AUDIT_LIMIT));
        assert!(query.principal.is_none());
        assert!(!query.refusals_only);
    }

    #[test]
    fn a_period_longer_than_the_clock_selects_nothing_rather_than_everything() {
        // The failure this excludes: `aik audit prune --older-than 52w` on a machine whose
        // clock has just been reset, wrapping past every record and taking the lot.
        assert_eq!(
            cutoff(Timestamp::from_millis(10), std::time::Duration::MAX),
            Timestamp::from_millis(0)
        );
    }
}
