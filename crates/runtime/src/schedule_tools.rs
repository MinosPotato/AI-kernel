//! Scheduling as something an agent can reach: three [`Tool`](aik_api::tool::Tool)s over the
//! existing [`Scheduler`].
//!
//! # Why this is here rather than in `aik-scheduler`
//!
//! [`aik_scheduler`] knows nothing about agents or prompts: a [`JobSpec::payload`] is opaque
//! to it, and a [`JobSpec::handler`] is just a [`ComponentId`] it calls back into. Deciding
//! *what a scheduled job actually does* is application wiring, not scheduling — the same
//! reason [`AgentJobComponent`](crate::AgentJobComponent) lives here and not there. These
//! tools are the other half of that wiring: they let a conversation create the jobs
//! [`AgentJobHandler`](crate::AgentJobHandler) runs, instead of only a deployment's own
//! configuration being able to.
//!
//! # The three tools
//!
//! | Tool | Permission | Resource claimed | Mutates |
//! |------|------------|------------------|---------|
//! | [`ScheduleCreateTool`] | `schedule.create` | `job/<id>` | yes |
//! | [`ScheduleListTool`] | `schedule.list` | [`ANY_JOB_RESOURCE`] | no |
//! | [`ScheduleCancelTool`] | `schedule.cancel` | `job/<id>` | yes |
//!
//! Three tools rather than one `schedule` tool with an `operation` argument, for the same
//! reason [`aik_memory`](https://docs.rs/aik-memory)'s tools are split: a deployment that
//! wants an agent to see and cancel its own reminders without being able to create new ones
//! registers only [`ScheduleListTool`] and [`ScheduleCancelTool`], and a policy wanting the
//! same guarantee a second way denies `schedule.create`.
//!
//! # Security
//!
//! * **The owner is never an argument.** No input here has an `owner` or `principal` field;
//!   [`Scheduler::schedule`] stamps the owner from the [`ExecutionContext`] the registry
//!   handed this tool, exactly as [`aik_memory`](https://docs.rs/aik-memory)'s tools leave
//!   ownership to the store.
//! * **The handler is fixed, not model-chosen.** [`JobSpec::handler`] names *any* registered
//!   [`JobHandler`](aik_api::scheduler::JobHandler) in the kernel, which could include
//!   handlers with no business being aimed at a model's whim. [`ScheduleCreateTool`] has no
//!   `handler` argument at all: every job it creates targets one fixed component, set once by
//!   [`ScheduleToolsComponent::with_handler`] and never read from a call's arguments.
//! * **`Trigger::OnEvent` is not offered.** The other four variants describe *when*; this one
//!   describes reacting to an internal kernel event by name, which is a wiring concern, not a
//!   reminder a conversation should be naming blind.
//! * **Every write is bounded.** [`ScheduleCreateTool`] refuses a prompt over
//!   [`MAX_PROMPT_BYTES`] and a job id over [`MAX_JOB_ID_LENGTH`], for the reason
//!   [`aik_memory`](https://docs.rs/aik-memory) bounds a record: so one call cannot fill a
//!   durable store or a context window.
//! * **An unwired tool refuses.** A tool whose [`ScheduleToolsComponent`] was never added to
//!   the kernel has no scheduler bound, and says so instead of doing anything.
//!
//! # Wiring
//!
//! ```no_run
//! use aik_core::prelude::*;
//! use aik_runtime::{AgentJobComponent, ScheduleToolsComponent};
//! use aik_scheduler::SchedulerComponent;
//! use aik_tools::ToolsComponent;
//!
//! # fn build() -> Result<Kernel> {
//! let schedule_tools = ScheduleToolsComponent::new();
//!
//! Kernel::builder()
//!     .component(SchedulerComponent::new())
//!     .component(AgentJobComponent::new())
//!     .component(
//!         ToolsComponent::new()
//!             .with_tool(schedule_tools.create())
//!             .with_tool(schedule_tools.list())
//!             .with_tool(schedule_tools.cancel()),
//!     )
//!     .component(schedule_tools)
//!     .build()
//! # }
//! ```

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use aik_api::agent::SessionId;
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::scheduler::{JobId, JobSpec, Scheduler, Trigger};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::clock::{Clock, SharedClock, Timestamp};
use aik_core::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The component id used when none is given explicitly.
pub const DEFAULT_TOOLS_COMPONENT_ID: &str = "schedule.tools";

/// The name [`ScheduleCreateTool`] registers under when none is given explicitly.
pub const DEFAULT_CREATE_NAME: &str = "schedule.create";

/// The permission [`ScheduleCreateTool`] requires when none is given explicitly.
pub const DEFAULT_CREATE_PERMISSION: &str = "schedule.create";

/// The name [`ScheduleListTool`] registers under when none is given explicitly.
pub const DEFAULT_LIST_NAME: &str = "schedule.list";

/// The permission [`ScheduleListTool`] requires when none is given explicitly.
pub const DEFAULT_LIST_PERMISSION: &str = "schedule.list";

/// The name [`ScheduleCancelTool`] registers under when none is given explicitly.
pub const DEFAULT_CANCEL_NAME: &str = "schedule.cancel";

/// The permission [`ScheduleCancelTool`] requires when none is given explicitly.
pub const DEFAULT_CANCEL_PERMISSION: &str = "schedule.cancel";

/// Prefix of the [`ResourceId`] one named job is authorized under, e.g. `job/nightly`.
pub const JOB_RESOURCE_PREFIX: &str = "job/";

/// The resource a [`ScheduleListTool`] call is authorized under.
///
/// A list names no job in particular, so it cannot honestly be authorized as one; this is
/// what a policy allowing `job/*` (or `*`) grants and a narrower one does not. See
/// [`aik_memory`](https://docs.rs/aik-memory)'s `ANY_KIND_RESOURCE` for the same reasoning.
pub const ANY_JOB_RESOURCE: &str = "job/*";

/// The longest job id [`ScheduleCreateTool`] accepts.
///
/// A job id is a short, stable name a caller chooses to refer back to a reminder, not a
/// payload; it reaches a [`ResourceId`] and a durable key verbatim.
pub const MAX_JOB_ID_LENGTH: usize = 128;

/// The largest prompt one [`ScheduleCreateTool`] call will store.
///
/// Generous for what a reminder is — a sentence or two telling the agent what to do when it
/// fires — and small enough that one call cannot park an unbounded amount of text in a
/// durable schedule.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// Validates a model-supplied job id, or explains why it is not one.
fn parse_job_id(raw: &str) -> Result<JobId> {
    if raw.is_empty() {
        return Err(Error::InvalidArgument("`id` must not be empty".to_owned()));
    }
    if raw.trim() != raw {
        return Err(Error::InvalidArgument(
            "`id` must not begin or end with whitespace".to_owned(),
        ));
    }
    if raw.len() > MAX_JOB_ID_LENGTH {
        return Err(Error::InvalidArgument(format!(
            "`id` must be at most {MAX_JOB_ID_LENGTH} bytes, got {}",
            raw.len()
        )));
    }
    if raw.chars().any(char::is_control) {
        return Err(Error::InvalidArgument(
            "`id` must not contain control characters".to_owned(),
        ));
    }
    Ok(JobId::new(raw))
}

/// The resource a given job id is authorized under.
fn job_resource(id: &JobId) -> ResourceId {
    ResourceId::new(format!("{JOB_RESOURCE_PREFIX}{id}"))
}

/// Refuses to start work whose context has already been cancelled or run out of time. See
/// [`aik_memory`](https://docs.rs/aik-memory)'s identical helper for why this exists.
fn ensure_live(cx: &ExecutionContext, clock: &dyn Clock) -> Result<()> {
    if cx.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if cx.deadline.is_some_and(|deadline| clock.now() >= deadline) {
        return Err(Error::Timeout(Duration::ZERO));
    }
    Ok(())
}

/// A [`Trigger`] as a model may express it.
///
/// [`Trigger::OnEvent`] is deliberately absent — see the module's security section — and
/// every duration is in seconds rather than milliseconds, because seconds are what a prompt
/// asking for "in ten minutes" or "every hour" naturally produces.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TriggerInput {
    /// Once, at an absolute time.
    At {
        /// Unix time, in seconds.
        unix_seconds: u64,
    },
    /// Once, after a delay from now.
    After {
        /// How long to wait, in seconds.
        delay_seconds: u64,
    },
    /// Repeatedly, at a fixed interval.
    Every {
        /// The interval, in seconds.
        interval_seconds: u64,
    },
    /// On a cron expression. See `aik_scheduler::cron` for the dialect.
    Cron {
        /// The expression.
        expression: String,
    },
}

impl TriggerInput {
    fn into_trigger(self) -> Result<Trigger> {
        Ok(match self {
            TriggerInput::At { unix_seconds } => {
                let millis = unix_seconds.checked_mul(1000).ok_or_else(|| {
                    Error::InvalidArgument(
                        "`unix_seconds` is too far in the future to represent".to_owned(),
                    )
                })?;
                Trigger::At {
                    timestamp: Timestamp::from_millis(millis),
                }
            }
            TriggerInput::After { delay_seconds } => Trigger::After {
                delay: Duration::from_secs(delay_seconds),
            },
            TriggerInput::Every { interval_seconds } => Trigger::Every {
                interval: Duration::from_secs(interval_seconds),
            },
            TriggerInput::Cron { expression } => Trigger::Cron { expression },
        })
    }
}

/// The [`Scheduler`] every schedule tool shares, filled in during component `init`. See
/// [`aik_memory`](https://docs.rs/aik-memory)'s `MemoryToolBinding` for why this exists and
/// why it is a binding rather than a service locator.
///
/// # Why a [`Weak`], not an [`Arc`]
///
/// A scheduler is not a leaf the way a memory store is: a persistent [`Scheduler`] holds
/// every registered [`JobHandler`](aik_api::scheduler::JobHandler) — in this deployment,
/// [`AgentJobHandler`](crate::AgentJobHandler), which holds `dyn Agent`, which holds the very
/// [`ToolRegistry`](aik_api::tool::ToolRegistry) this tool is registered in. A tool that held
/// its scheduler by [`Arc`] would close that loop —
/// registry → this tool → scheduler → job handler → agent → registry — and nothing in `Arc`
/// detects a cycle, so the whole chain would outlive the kernel that built it. A [`Weak`]
/// breaks it: the scheduler is kept alive by [`SchedulerComponent`](aik_scheduler::SchedulerComponent)
/// itself for exactly as long as the kernel runs, which is the only lifetime this tool ever
/// needed anyway.
struct ScheduleToolBinding {
    bound: OnceLock<(Weak<dyn Scheduler>, SharedClock)>,
}

impl std::fmt::Debug for ScheduleToolBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleToolBinding")
            .field("bound", &self.bound.get().is_some())
            .finish()
    }
}

impl ScheduleToolBinding {
    fn new() -> Self {
        Self {
            bound: OnceLock::new(),
        }
    }

    fn bind(&self, scheduler: Arc<dyn Scheduler>, clock: SharedClock) -> Result<()> {
        self.bound
            .set((Arc::downgrade(&scheduler), clock))
            .map_err(|_| Error::Lifecycle("schedule tools are already bound".to_owned()))
    }

    /// Upgrades to the live scheduler, or explains why there is none.
    ///
    /// A failed upgrade after a successful [`bind`](Self::bind) means the kernel that
    /// published it has already been torn down — the same shape of refusal an unbound tool
    /// gives, for the same reason: there is nothing left to ask.
    fn scheduler(&self) -> Result<Arc<dyn Scheduler>> {
        self.bound()?.0.upgrade().ok_or_else(|| {
            Error::Lifecycle("the scheduler this tool was bound to is no longer running".to_owned())
        })
    }

    fn clock(&self) -> Result<&SharedClock> {
        Ok(&self.bound()?.1)
    }

    fn bound(&self) -> Result<&(Weak<dyn Scheduler>, SharedClock)> {
        self.bound.get().ok_or_else(|| {
            Error::Lifecycle(
                "schedule tools are not bound to a scheduler; add `ScheduleToolsComponent` to \
                 the kernel"
                    .to_owned(),
            )
        })
    }
}

/// Schedules an agent prompt to run later, once or repeatedly.
///
/// # What a model may and may not set
///
/// | Field | From |
/// |-------|------|
/// | `id`, `trigger`, `prompt`, `session`, `persistent` | the model (validated and bounded) |
/// | `handler` | fixed at construction, by [`ScheduleToolsComponent::with_handler`] |
/// | owner | the [`ExecutionContext`], never an argument |
///
/// Naming an existing `id` replaces that job, exactly as [`Scheduler::schedule`] documents —
/// refused unless the caller may act for whoever owns it.
pub struct ScheduleCreateTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<ScheduleToolBinding>,
    handler: ComponentId,
    max_prompt_bytes: usize,
}

impl std::fmt::Debug for ScheduleCreateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleCreateTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .field("handler", &self.handler)
            .field("max_prompt_bytes", &self.max_prompt_bytes)
            .finish()
    }
}

impl ScheduleCreateTool {
    fn new(binding: Arc<ScheduleToolBinding>, handler: ComponentId) -> Self {
        Self {
            name: ToolName::new(DEFAULT_CREATE_NAME),
            action: ActionId::new(DEFAULT_CREATE_PERMISSION),
            binding,
            handler,
            max_prompt_bytes: MAX_PROMPT_BYTES,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_CREATE_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    /// Overrides the maximum size of a single stored prompt.
    #[must_use]
    pub fn with_max_prompt_bytes(mut self, max_prompt_bytes: usize) -> Self {
        self.max_prompt_bytes = max_prompt_bytes;
        self
    }

    fn parse(&self, arguments: Value) -> Result<CreateInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

fn default_persistent() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    id: String,
    trigger: TriggerInput,
    prompt: String,
    #[serde(default)]
    session: Option<SessionId>,
    #[serde(default = "default_persistent")]
    persistent: bool,
}

#[async_trait]
impl Tool for ScheduleCreateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Schedules a prompt to be asked of you later, once or repeatedly, \
                          without anybody there to type it. `id` names the reminder, so \
                          scheduling again with the same `id` replaces it. `persistent` \
                          (default true) asks for it to survive a restart; some deployments \
                          cannot offer that and will refuse rather than silently forgetting."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Name for this reminder. Reusing an id replaces it.",
                        "maxLength": MAX_JOB_ID_LENGTH
                    },
                    "trigger": {
                        "type": "object",
                        "description": "When to fire.",
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "at" },
                                    "unix_seconds": { "type": "integer", "minimum": 0 }
                                },
                                "required": ["type", "unix_seconds"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "after" },
                                    "delay_seconds": { "type": "integer", "minimum": 0 }
                                },
                                "required": ["type", "delay_seconds"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "every" },
                                    "interval_seconds": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["type", "interval_seconds"]
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "cron" },
                                    "expression": { "type": "string" }
                                },
                                "required": ["type", "expression"]
                            }
                        ]
                    },
                    "prompt": {
                        "type": "string",
                        "description": "What to ask yourself when this fires.",
                        "maxLength": self.max_prompt_bytes
                    },
                    "session": {
                        "type": "string",
                        "description": "Id of an existing conversation to append the firing \
                                        to. Omit for a fresh one each time."
                    },
                    "persistent": {
                        "type": "boolean",
                        "description": "Survive a restart. Defaults to true."
                    }
                },
                "required": ["id", "trigger", "prompt"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "persistent": { "type": "boolean" },
                    "error": { "type": "string" }
                },
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let id = parse_job_id(&input.id)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            job_resource(&id),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;
        let id = parse_job_id(&input.id)?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        if input.prompt.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "`prompt` must not be empty".to_owned(),
            ));
        }
        if input.prompt.len() > self.max_prompt_bytes {
            return Ok(ToolOutcome::error(json!({
                "error": format!(
                    "prompt is {} bytes; the limit is {} — ask for something shorter",
                    input.prompt.len(),
                    self.max_prompt_bytes
                )
            })));
        }

        let trigger = input.trigger.into_trigger()?;
        let mut payload = Map::new();
        payload.insert("prompt".to_owned(), json!(input.prompt));
        if let Some(session) = input.session {
            payload.insert("session".to_owned(), json!(session.to_string()));
        }

        let spec = JobSpec::new(id.clone(), trigger, self.handler.clone())
            .with_payload(Value::Object(payload))
            .persistent(input.persistent);

        let scheduler = self.binding.scheduler()?;
        scheduler.schedule(spec, cx).await?;

        Ok(ToolOutcome::ok(json!({
            "id": id.to_string(),
            "persistent": input.persistent
        })))
    }
}

/// Lists the scheduled prompts the caller may act for.
pub struct ScheduleListTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<ScheduleToolBinding>,
}

impl std::fmt::Debug for ScheduleListTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleListTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .finish()
    }
}

impl ScheduleListTool {
    fn new(binding: Arc<ScheduleToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_LIST_NAME),
            action: ActionId::new(DEFAULT_LIST_PERMISSION),
            binding,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_LIST_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }
}

#[async_trait]
impl Tool for ScheduleListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Lists your scheduled prompts: what they ask, when they next fire, \
                          and whether they survive a restart."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "jobs": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "trigger": { "type": "object" },
                                "payload": {},
                                "persistent": { "type": "boolean" },
                                "next_run": { "type": "integer" },
                                "last_run": { "type": "integer" }
                            },
                            "required": ["id", "trigger", "payload", "persistent"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["count", "jobs"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: true,
        }
    }

    fn planned_resources(&self, _arguments: &Value) -> Result<Vec<ResourceClaim>> {
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            ResourceId::new(ANY_JOB_RESOURCE),
        )])
    }

    async fn invoke(
        &self,
        _arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        ensure_live(cx, self.binding.clock()?.as_ref())?;
        let scheduler = self.binding.scheduler()?;
        let jobs = scheduler.list(cx).await?;

        let rendered: Vec<Value> = jobs
            .iter()
            .map(|job| {
                let mut rendered = Map::new();
                rendered.insert("id".to_owned(), json!(job.spec.id.to_string()));
                rendered.insert("trigger".to_owned(), json!(job.spec.trigger));
                rendered.insert("payload".to_owned(), job.spec.payload.clone());
                rendered.insert("persistent".to_owned(), json!(job.spec.persistent));
                if let Some(next_run) = job.next_run {
                    rendered.insert("next_run".to_owned(), json!(next_run.as_millis()));
                }
                if let Some(last_run) = job.last_run {
                    rendered.insert("last_run".to_owned(), json!(last_run.as_millis()));
                }
                Value::Object(rendered)
            })
            .collect();

        Ok(ToolOutcome::ok(json!({
            "count": rendered.len(),
            "jobs": rendered
        })))
    }
}

/// Cancels one scheduled prompt, by id.
///
/// Reports whether there was one to cancel, rather than treating "no such job" as an error —
/// the same distinction [`Scheduler::cancel`] draws.
pub struct ScheduleCancelTool {
    name: ToolName,
    action: ActionId,
    binding: Arc<ScheduleToolBinding>,
}

impl std::fmt::Debug for ScheduleCancelTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleCancelTool")
            .field("name", &self.name)
            .field("action", &self.action)
            .finish()
    }
}

impl ScheduleCancelTool {
    fn new(binding: Arc<ScheduleToolBinding>) -> Self {
        Self {
            name: ToolName::new(DEFAULT_CANCEL_NAME),
            action: ActionId::new(DEFAULT_CANCEL_PERMISSION),
            binding,
        }
    }

    /// Registers under a different tool name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Requires a different permission than [`DEFAULT_CANCEL_PERMISSION`].
    #[must_use]
    pub fn with_permission(mut self, action: impl Into<ActionId>) -> Self {
        self.action = action.into();
        self
    }

    fn parse(&self, arguments: Value) -> Result<IdInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdInput {
    id: String,
}

#[async_trait]
impl Tool for ScheduleCancelTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Cancels one of your scheduled prompts, by id. Reports whether \
                          there was one to cancel."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Id of the reminder to cancel."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "cancelled": { "type": "boolean" }
                },
                "required": ["id", "cancelled"],
                "additionalProperties": false
            })),
            required_permissions: vec![self.action.clone()],
            read_only: false,
        }
    }

    fn planned_resources(&self, arguments: &Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        let id = parse_job_id(&input.id)?;
        Ok(vec![ResourceClaim::new(
            self.action.clone(),
            job_resource(&id),
        )])
    }

    async fn invoke(
        &self,
        arguments: Value,
        _authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;
        let id = parse_job_id(&input.id)?;
        ensure_live(cx, self.binding.clock()?.as_ref())?;

        let scheduler = self.binding.scheduler()?;
        let cancelled = scheduler.cancel(&id, cx).await?;

        Ok(ToolOutcome::ok(json!({
            "id": id.to_string(),
            "cancelled": cancelled
        })))
    }
}

/// Binds the three schedule tools to a [`Scheduler`] published by another component.
///
/// See [`aik_memory`](https://docs.rs/aik-memory)'s `MemoryToolsComponent` for why a
/// component of its own is what bridges "tools are built before the kernel" and "the
/// scheduler is published during `init`".
pub struct ScheduleToolsComponent {
    id: ComponentId,
    scheduler: ComponentId,
    handler: ComponentId,
    binding: Arc<ScheduleToolBinding>,
}

impl std::fmt::Debug for ScheduleToolsComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleToolsComponent")
            .field("id", &self.id)
            .field("scheduler", &self.scheduler)
            .field("handler", &self.handler)
            .finish()
    }
}

impl Default for ScheduleToolsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduleToolsComponent {
    /// Creates a component registered under [`DEFAULT_TOOLS_COMPONENT_ID`], binding to the
    /// scheduler published under [`aik_scheduler`]'s own default id and targeting
    /// [`crate::jobs::DEFAULT_COMPONENT_ID`] — the agent job handler — for every job it
    /// creates.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_TOOLS_COMPONENT_ID),
            scheduler: ComponentId::new(aik_scheduler::DEFAULT_COMPONENT_ID),
            handler: ComponentId::new(crate::jobs::DEFAULT_COMPONENT_ID),
            binding: Arc::new(ScheduleToolBinding::new()),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Binds to the scheduler published by a differently named component.
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: impl Into<ComponentId>) -> Self {
        self.scheduler = scheduler.into();
        self
    }

    /// Targets a differently named job handler for every job [`ScheduleCreateTool`] creates.
    ///
    /// The handler this names must be registered as a `dyn JobHandler` — normally
    /// [`crate::AgentJobComponent`] — or a job created here will fail the moment it fires,
    /// with no handler there to run it.
    #[must_use]
    pub fn with_handler(mut self, handler: impl Into<ComponentId>) -> Self {
        self.handler = handler.into();
        self
    }

    /// A tool that schedules an agent prompt. See [`ScheduleCreateTool`].
    pub fn create(&self) -> ScheduleCreateTool {
        ScheduleCreateTool::new(self.binding.clone(), self.handler.clone())
    }

    /// A tool that lists scheduled prompts. See [`ScheduleListTool`].
    pub fn list(&self) -> ScheduleListTool {
        ScheduleListTool::new(self.binding.clone())
    }

    /// A tool that cancels a scheduled prompt. See [`ScheduleCancelTool`].
    pub fn cancel(&self) -> ScheduleCancelTool {
        ScheduleCancelTool::new(self.binding.clone())
    }
}

#[async_trait]
impl Component for ScheduleToolsComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("binds the schedule tools to a scheduler")
            .requires(self.scheduler.clone())
            .optionally_requires(self.handler.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let scheduler = ctx.service_named::<dyn Scheduler>(&self.scheduler)?;
        self.binding.bind(scheduler, ctx.clock().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{Principal, PrincipalKind};
    use aik_scheduler::SchedulerComponent;

    fn cx(principal: &str) -> ExecutionContext {
        ExecutionContext::new().with_principal(Principal::new(principal, PrincipalKind::User))
    }

    #[derive(Debug)]
    struct NoDiscoveredResources;

    #[async_trait]
    impl ResourceAuthorizer for NoDiscoveredResources {
        async fn authorize(&self, _action: &ActionId, _resource: &ResourceId) -> Result<()> {
            unreachable!("these tools declare every resource they touch in advance")
        }
    }

    async fn invoke(
        tool: &impl Tool,
        arguments: Value,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        tool.invoke(arguments, &NoDiscoveredResources, cx).await
    }

    #[test]
    fn it_depends_on_the_scheduler_and_optionally_on_its_handler() {
        let component = ScheduleToolsComponent::new().with_scheduler("scheduler.secondary");
        let descriptor = component.descriptor();
        let required: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(required, vec![&ComponentId::new("scheduler.secondary")]);
        assert!(
            descriptor
                .dependencies
                .iter()
                .any(|dependency| dependency.optional
                    && dependency.id == ComponentId::new(crate::jobs::DEFAULT_COMPONENT_ID))
        );
    }

    #[test]
    fn the_tools_it_hands_out_have_the_expected_names_and_permissions() {
        let component = ScheduleToolsComponent::new();
        let specs = [
            component.create().spec(),
            component.list().spec(),
            component.cancel().spec(),
        ];
        let names: Vec<String> = specs.iter().map(|spec| spec.name.to_string()).collect();
        assert_eq!(
            names,
            vec!["schedule.create", "schedule.list", "schedule.cancel"]
        );
        for spec in &specs {
            assert_eq!(
                spec.required_permissions,
                vec![ActionId::new(spec.name.as_str())],
                "`{}` should require exactly its own permission",
                spec.name
            );
        }
        assert!(!specs[0].read_only);
        assert!(specs[1].read_only);
        assert!(!specs[2].read_only);
    }

    #[test]
    fn a_trigger_with_no_type_or_an_unknown_one_is_refused() {
        let component = ScheduleToolsComponent::new();
        let tool = component.create();
        for arguments in [
            json!({ "id": "x", "prompt": "hi", "trigger": { "type": "on_event", "event": "x" } }),
            json!({ "id": "x", "prompt": "hi", "trigger": {} }),
        ] {
            assert!(
                tool.parse(arguments.clone()).is_err(),
                "{arguments} should not parse"
            );
        }
    }

    #[test]
    fn an_out_of_range_job_id_is_refused() {
        let component = ScheduleToolsComponent::new();
        let tool = component.create();
        let arguments = json!({
            "id": "",
            "prompt": "hi",
            "trigger": { "type": "after", "delay_seconds": 10 }
        });
        assert!(tool.planned_resources(&arguments).is_err());

        let arguments = json!({
            "id": "x".repeat(MAX_JOB_ID_LENGTH + 1),
            "prompt": "hi",
            "trigger": { "type": "after", "delay_seconds": 10 }
        });
        assert!(tool.planned_resources(&arguments).is_err());
    }

    #[tokio::test]
    async fn init_binds_the_tools_to_the_published_scheduler() {
        let component = ScheduleToolsComponent::new();
        let create = component.create();
        let kernel = Kernel::builder()
            .component(SchedulerComponent::new())
            .component(component)
            .build()
            .expect("a valid kernel");
        kernel.start().await.expect("the kernel starts");

        let outcome = invoke(
            &create,
            json!({
                "id": "nightly",
                "prompt": "say hi",
                "trigger": { "type": "every", "interval_seconds": 3600 },
                "persistent": false
            }),
            &cx("alice"),
        )
        .await
        .expect("the tool is bound");
        assert!(!outcome.is_error);

        kernel.shutdown().await.expect("the kernel stops");
    }

    #[tokio::test]
    async fn a_tool_whose_component_was_never_added_refuses() {
        let component = ScheduleToolsComponent::new();
        let create = component.create();
        drop(component);

        let error = invoke(
            &create,
            json!({
                "id": "nightly",
                "prompt": "say hi",
                "trigger": { "type": "after", "delay_seconds": 1 }
            }),
            &cx("alice"),
        )
        .await
        .expect_err("nothing bound the tool");
        assert_eq!(error.kind(), aik_core::ErrorKind::Lifecycle);
    }

    #[tokio::test]
    async fn list_and_cancel_round_trip_through_a_real_scheduler() {
        let component = ScheduleToolsComponent::new();
        let create = component.create();
        let list = component.list();
        let cancel = component.cancel();
        let kernel = Kernel::builder()
            .component(SchedulerComponent::new())
            .component(component)
            .build()
            .expect("a valid kernel");
        kernel.start().await.expect("the kernel starts");
        let alice = cx("alice");

        invoke(
            &create,
            json!({
                "id": "nightly",
                "prompt": "say hi",
                "trigger": { "type": "every", "interval_seconds": 3600 },
                "persistent": false
            }),
            &alice,
        )
        .await
        .expect("scheduled");

        let listed = invoke(&list, json!({}), &alice).await.expect("listed");
        assert_eq!(listed.output["count"], json!(1));
        assert_eq!(listed.output["jobs"][0]["id"], json!("nightly"));

        let cancelled = invoke(&cancel, json!({ "id": "nightly" }), &alice)
            .await
            .expect("cancelled");
        assert_eq!(cancelled.output["cancelled"], json!(true));

        let listed_again = invoke(&list, json!({}), &alice).await.expect("listed");
        assert_eq!(listed_again.output["count"], json!(0));

        kernel.shutdown().await.expect("the kernel stops");
    }

    #[tokio::test]
    async fn a_job_someone_else_owns_cannot_be_cancelled() {
        let component = ScheduleToolsComponent::new();
        let create = component.create();
        let cancel = component.cancel();
        let kernel = Kernel::builder()
            .component(SchedulerComponent::new())
            .component(component)
            .build()
            .expect("a valid kernel");
        kernel.start().await.expect("the kernel starts");

        invoke(
            &create,
            json!({
                "id": "nightly",
                "prompt": "say hi",
                "trigger": { "type": "after", "delay_seconds": 3600 },
                "persistent": false
            }),
            &cx("alice"),
        )
        .await
        .expect("scheduled");

        let error = invoke(&cancel, json!({ "id": "nightly" }), &cx("mallory"))
            .await
            .expect_err("mallory does not own alice's job");
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);

        kernel.shutdown().await.expect("the kernel stops");
    }
}
