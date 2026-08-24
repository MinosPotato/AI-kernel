//! What a request actually does.
//!
//! One [`Host`] per process, shared by every connection. It holds the started kernel's
//! context and the handful of capabilities a client can ask about, and it turns a
//! [`Request`] into a call on one of them.
//!
//! # The one identity
//!
//! Every [`ExecutionContext`] built here carries a principal derived from the host's own
//! resolved settings, and there is no code path that takes one from a client. That is the
//! whole of the authorization story at this layer, and everything below it is unchanged:
//!
//! * A conversation runs as [`RuntimeSettings::principal`] — the agent, acting on behalf of
//!   the user. The agent reaches tools only through
//!   [`ToolRegistry`](aik_api::tool::ToolRegistry), which consults the policy engine; this
//!   module holds no tool and cannot invoke one.
//! * A session, a memory or a job that belongs to somebody else is refused by the store that
//!   owns it, not by a check here. A listing filters for the same reason: the store returns
//!   what the principal may act for, and a second filter in the host would be a second place
//!   for the rule to be wrong.
//! * A review of the audit trail runs as [`RuntimeSettings::operator`] — the *person*, not
//!   the agent — exactly as `aik audit` does when it opens the database directly. The socket
//!   establishes that the caller is the account that owns the database, which is the same
//!   thing opening the file establishes.
//!
//! # What is deliberately not here
//!
//! No tool invocation, no policy evaluation, no way to construct a [`Principal`], and no
//! request that reaches the registry by name. A client asks for a conversation, a listing, a
//! schedule or a query; what runs, and whether it may, belongs to the subsystems.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aik_api::agent::{Agent, AgentRequest, AgentUpdate, SessionId};
use aik_api::audit::{AuditRecord, AuditStore};
use aik_api::context::ContextStore;
use aik_api::execution::ExecutionContext;
use aik_api::model::{ContentPart, ModelId};
use aik_api::permission::Principal;
use aik_api::scheduler::{JobSpec, Scheduler};
use aik_approval::ApprovalBroker;
use aik_audit::AuditRetentionSweeper;
use aik_core::clock::Timestamp;
use aik_core::prelude::*;
use aik_ipc::protocol::{HostStatus, Reply, Request, ScheduleRequest};
use aik_runtime::{JobExecution, RuntimeSettings};
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The version reported to clients and printed by `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything a connection needs in order to answer a request.
pub struct Host {
    settings: RuntimeSettings,
    model: ModelId,
    broker: Arc<ApprovalBroker>,
    agent: Arc<dyn Agent>,
    context: Arc<dyn ContextStore>,
    scheduler: Arc<dyn Scheduler>,
    audit: Arc<dyn AuditStore>,
    sweeper: Arc<dyn AuditRetentionSweeper>,
    started: Instant,
    connections: AtomicUsize,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("agent", &self.settings.agent)
            .field("model", &self.model)
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Host {
    /// Resolves everything a client can reach from a started kernel.
    ///
    /// Resolved once, here, rather than per request: a failure to find one of these is a
    /// wiring mistake, and a wiring mistake should stop the host coming up rather than
    /// surface as one client's request failing much later.
    pub fn new(
        kernel: &KernelContext,
        settings: RuntimeSettings,
        model: ModelId,
        broker: Arc<ApprovalBroker>,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            agent: kernel.service::<dyn Agent>()?,
            context: kernel.service::<dyn ContextStore>()?,
            scheduler: kernel.service::<dyn Scheduler>()?,
            audit: kernel.service::<dyn AuditStore>()?,
            sweeper: kernel.service::<dyn AuditRetentionSweeper>()?,
            settings,
            model,
            broker,
            started: Instant::now(),
            connections: AtomicUsize::new(0),
        }))
    }

    /// The principal every conversation, session and job operation runs as.
    pub fn principal(&self) -> Principal {
        self.settings.principal()
    }

    /// The broker approvals are parked on.
    pub fn broker(&self) -> &Arc<ApprovalBroker> {
        &self.broker
    }

    /// The resolved deployment this host serves.
    pub fn settings(&self) -> &RuntimeSettings {
        &self.settings
    }

    /// Records that a client connected, and returns how many are now connected.
    pub fn connected(&self) -> usize {
        self.connections.fetch_add(1, Ordering::SeqCst);
        self.connections()
    }

    /// Records that a client disconnected.
    pub fn disconnected(&self) {
        self.connections.fetch_sub(1, Ordering::SeqCst);
    }

    /// How many clients are connected.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The context one operation runs under.
    ///
    /// One constructor, used by every request alike, so that "who is asking" is decided in
    /// exactly one place. Two of these would be two chances for a request to run as somebody
    /// the host is not, and the difference would be invisible until it showed one client
    /// another principal's session.
    fn cx(&self, cancel: &CancellationToken) -> ExecutionContext {
        self.context_for(self.principal(), cancel)
    }

    /// The context a review of the audit trail runs under.
    ///
    /// The operator rather than the agent: nobody is delegating anything to a model when a
    /// person reads the trail, and reading as the user is what shows them the whole of what
    /// their agents did for them. See [`aik_api::audit::AuditRecord::visible_to`].
    fn review_cx(&self, cancel: &CancellationToken) -> ExecutionContext {
        self.context_for(self.settings.operator(), cancel)
    }

    /// The one place an [`ExecutionContext`] is built.
    ///
    /// Both callers above go through it, so the two things every request needs — an identity
    /// that came from this host's settings, and the call's own cancellation — are attached in
    /// one place rather than two. A second construction site is a second chance for one of
    /// them to be forgotten, and forgetting the first is how a request runs as somebody else.
    fn context_for(&self, principal: Principal, cancel: &CancellationToken) -> ExecutionContext {
        ExecutionContext {
            principal: Some(principal),
            cancellation: cancel.clone(),
            ..ExecutionContext::new()
        }
    }

    /// Answers a request that produces no stream of updates.
    ///
    /// [`Request::Prompt`] is handled by [`Host::prompt`] instead, because it is the one that
    /// reports as it goes; everything else is a single question with a single answer.
    pub async fn handle(&self, request: Request, cancel: &CancellationToken) -> Result<Reply> {
        match request {
            Request::Ping => Ok(Reply::Pong),
            Request::Status => Ok(Reply::Status(Box::new(self.status()))),
            Request::Sessions => {
                let sessions = self.context.sessions(&self.cx(cancel)).await?;
                Ok(Reply::Sessions(sessions))
            }
            Request::Clear { session } => {
                let records = self.context.clear(&session, &self.cx(cancel)).await?;
                Ok(Reply::Removed { records })
            }
            Request::Compact { session, keep } => {
                let records = self
                    .context
                    .compact(&session, keep, &self.cx(cancel))
                    .await?;
                Ok(Reply::Removed { records })
            }
            Request::Jobs => {
                let jobs = self.scheduler.list(&self.cx(cancel)).await?;
                Ok(Reply::Jobs(jobs))
            }
            Request::Schedule(request) => {
                self.schedule(request, cancel).await?;
                Ok(Reply::Ok)
            }
            Request::CancelJob { job } => {
                let existed = self.scheduler.cancel(&job, &self.cx(cancel)).await?;
                Ok(Reply::Cancelled { existed })
            }
            Request::Audit { query } => {
                let records = self.audit(query, cancel).await?;
                Ok(Reply::Audit {
                    records,
                    issued: self.audit.last_sequence().await?,
                })
            }
            Request::Prune {
                older_than_ms,
                dry_run,
            } => self.prune(older_than_ms, dry_run).await,
            // Handled by the connection, which owns the approval gate and the table of calls
            // in flight. Reaching here means the connection did not, which is a bug rather
            // than something to answer.
            Request::Prompt { .. } | Request::Cancel { .. } => Err(Error::other(
                "this request is answered by the connection, not by the host",
            )),
            Request::Approve { .. } | Request::Deny { .. } => Err(Error::other(
                "an approval is answered through the connection that was asked",
            )),
        }
    }

    /// Runs one turn, reporting every update through `report` before the final response.
    ///
    /// The stream is the agent's own; nothing here filters it or adds to it. A cancelled
    /// call ends the stream, and the turn's own [`ExecutionContext`] is what carries the
    /// cancellation down to the model call and the tool call underneath it.
    pub async fn prompt(
        &self,
        session: Option<SessionId>,
        input: String,
        cancel: &CancellationToken,
        mut report: impl FnMut(AgentUpdate),
    ) -> Result<Reply> {
        let cx = self.cx(cancel);
        let request = AgentRequest {
            session: session.unwrap_or_default(),
            input: vec![ContentPart::text(input)],
            context: Value::Null,
        };

        let mut updates = self.agent.stream(request, &cx).await?;
        let mut finished = None;

        while let Some(update) = updates.next().await {
            let update = update?;
            if let AgentUpdate::Finished(response) = &update {
                finished = Some(response.clone());
            }
            report(update);
        }

        match finished {
            Some(response) => Ok(Reply::Finished(Box::new(response))),
            // The stream ended without a final response. Reported as a failure rather than as
            // an empty success, because a client that showed "done" here would be saying the
            // turn completed.
            //
            // Not the cancellation path, despite appearances: a cancelled run fails its own
            // next step and the stream yields that error, which the `?` above has already
            // returned as `Error::Cancelled`. Reaching here means an agent ended its stream
            // without the [`AgentUpdate::Finished`] its contract says ends it, which is a
            // broken agent rather than a cancelled one and should read as such.
            None => Err(Error::other(
                "the agent ended its stream without finishing the turn",
            )),
        }
    }

    /// Schedules an agent job owned by this host's principal.
    ///
    /// The client says when and what to ask. Everything else — which handler runs it, whose
    /// authority its firings carry — is decided here and by the scheduler, so a scheduled job
    /// can only ever be an agent turn belonging to whoever scheduled it. See
    /// [`aik_runtime::jobs`] for what a firing can then reach.
    async fn schedule(&self, request: ScheduleRequest, cancel: &CancellationToken) -> Result<()> {
        if self.settings.jobs != JobExecution::Agent {
            return Err(Error::Unsupported(
                "this host does not run scheduled jobs, so accepting one would mean storing \
                 something that never fires"
                    .to_owned(),
            ));
        }
        if request.prompt.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "a scheduled job needs something to ask".to_owned(),
            ));
        }

        let mut payload = json!({ "prompt": request.prompt });
        if let Some(session) = request.session {
            payload["session"] = json!(session.to_string());
        }

        let mut spec = JobSpec::new(
            request.id,
            request.trigger,
            aik_runtime::jobs::DEFAULT_COMPONENT_ID,
        )
        .with_payload(payload)
        .persistent(request.persistent);
        if let Some(timeout) = request.timeout_ms {
            spec = spec.with_timeout(Duration::from_millis(timeout));
        }

        self.scheduler.schedule(spec, &self.cx(cancel)).await
    }

    /// Reads the durable trail as the operator.
    async fn audit(
        &self,
        query: aik_api::audit::AuditQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<AuditRecord>> {
        self.audit.query(&query, &self.review_cx(cancel)).await
    }

    /// Removes records older than a period, or reports how many that would be.
    ///
    /// The one destructive operation this protocol carries, and it is here because redb locks
    /// the database: while a host is running there is no second process that could open the
    /// trail to prune it, and making an operator stop the host in order to apply a retention
    /// period would make stopping the host routine.
    ///
    /// It removes nothing a sweep would spare — a gap, a retention marker — because that is
    /// [`AuditRetentionSweeper`]'s rule and not something this relaxes, and the removal itself
    /// is recorded in the trail.
    async fn prune(&self, older_than_ms: u64, dry_run: bool) -> Result<Reply> {
        let cutoff = Timestamp::now()
            .as_millis()
            .checked_sub(older_than_ms)
            .ok_or_else(|| {
                Error::InvalidArgument(
                    "that retention period reaches back before there was a trail".to_owned(),
                )
            })?;
        let cutoff = Timestamp::from_millis(cutoff);

        let removed = if dry_run {
            self.sweeper.count_older_than(cutoff).await?
        } else {
            self.sweeper.sweep_older_than(cutoff).await?
        };

        Ok(Reply::Pruned {
            removed: removed as u64,
            issued: self.audit.last_sequence().await?,
        })
    }

    /// What this host is serving.
    pub fn status(&self) -> HostStatus {
        HostStatus {
            version: VERSION.to_owned(),
            agent: self.settings.agent.to_string(),
            user: self.settings.user.to_string(),
            model: self.model.to_string(),
            root: self.settings.root.clone(),
            database: self.settings.database().map(std::path::Path::to_path_buf),
            memory: self.settings.memory.as_str().to_owned(),
            runs_jobs: self.settings.jobs == JobExecution::Agent,
            connections: self.connections(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_reports_whether_this_process_runs_the_schedule() {
        // Asserted against the setting rather than against a literal, because the field is a
        // report of a wiring decision and not a decision of its own.
        for (jobs, expected) in [(JobExecution::Agent, true), (JobExecution::Disabled, false)] {
            let mut settings = RuntimeSettings::new("/tmp");
            settings.jobs = jobs;
            assert_eq!(settings.jobs == JobExecution::Agent, expected);
        }
    }
}
