//! [`EchoTool`]: a minimal, harmless [`Tool`] that proves the registration → discovery →
//! authorization → execution → audit path end to end, without doing anything to the
//! outside world.

use std::time::{Duration, SystemTime};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer};
use aik_api::tool::{ResourceClaim, Tool, ToolName, ToolOutcome, ToolSpec};
use aik_core::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// The tool name used when none is given explicitly.
pub const DEFAULT_NAME: &str = "kernel.echo";

/// The permission required when none is given explicitly.
pub const DEFAULT_PERMISSION: &str = "kernel.echo";

#[derive(Debug, Deserialize)]
struct EchoInput {
    text: String,
    #[serde(default)]
    delay_ms: u64,
    /// A resource declared up front, authorized before the tool runs.
    #[serde(default)]
    resource: Option<String>,
    /// A resource "discovered" mid-run, authorized through the [`ResourceAuthorizer`].
    #[serde(default)]
    discovered_resource: Option<String>,
}

/// Echoes its input back, optionally after a delay.
///
/// This tool has no real-world effect — it exists purely to exercise the tool foundation:
///
/// * with just `text`, it proves registration, discovery, schema exposure, capability-level
///   authorization, structured results and audit events;
/// * with `resource`, it proves *resource-level* authorization: the value is declared from
///   [`Tool::planned_resources`] and authorized before the tool runs at all;
/// * with `discovered_resource`, it proves the third phase: the value is authorized from
///   inside [`Tool::invoke`] through the [`ResourceAuthorizer`], the way a real tool would
///   handle a symlink target or a directory entry it only found while running;
/// * with a non-zero `delay_ms`, it proves cancellation and deadline propagation, by racing
///   the delay against [`ExecutionContext::cancelled`] and the context deadline exactly the
///   way a real tool must.
///
/// A real filesystem tool would canonicalise a path in `planned_resources` rather than
/// passing a string through untouched; see the
/// [TOCTOU discussion](aik_api::tool#time-of-check-to-time-of-use) for why that matters and
/// why this tool is not a template for one.
#[derive(Debug, Clone)]
pub struct EchoTool {
    name: ToolName,
    required_permissions: Vec<ActionId>,
    resource_action: ActionId,
}

impl Default for EchoTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoTool {
    /// Creates a tool named [`DEFAULT_NAME`], requiring [`DEFAULT_PERMISSION`].
    pub fn new() -> Self {
        Self {
            name: ToolName::new(DEFAULT_NAME),
            required_permissions: vec![ActionId::new(DEFAULT_PERMISSION)],
            resource_action: ActionId::new(DEFAULT_PERMISSION),
        }
    }

    /// Registers under a different name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<ToolName>) -> Self {
        self.name = name.into();
        self
    }

    /// Overrides the required permissions — pass an empty list to require none.
    #[must_use]
    pub fn requiring(mut self, permissions: impl IntoIterator<Item = ActionId>) -> Self {
        self.required_permissions = permissions.into_iter().collect();
        self
    }

    /// Sets the action declared against `resource` and `discovered_resource`.
    ///
    /// Defaults to [`DEFAULT_PERMISSION`], so that by default the capability-level and
    /// resource-level questions concern the same action.
    #[must_use]
    pub fn with_resource_action(mut self, action: impl Into<ActionId>) -> Self {
        self.resource_action = action.into();
        self
    }

    fn parse(&self, arguments: serde_json::Value) -> Result<EchoInput> {
        serde_json::from_value(arguments).map_err(|error| {
            Error::InvalidArgument(format!("invalid arguments for `{}`: {error}", self.name))
        })
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Echoes the given text back, optionally after a delay in \
                          milliseconds. Has no effect outside this call; exists to prove \
                          the tool foundation works, not to do anything real."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "delay_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "How long to wait, in milliseconds, before replying."
                    },
                    "resource": {
                        "type": "string",
                        "description": "A resource to declare and have authorized before running."
                    },
                    "discovered_resource": {
                        "type": "string",
                        "description": "A resource to authorize from inside the call, as if \
                                        it had only been discovered while running."
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "resources": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["text"],
                "additionalProperties": false
            })),
            required_permissions: self.required_permissions.clone(),
            read_only: true,
        }
    }

    fn planned_resources(&self, arguments: &serde_json::Value) -> Result<Vec<ResourceClaim>> {
        let input = self.parse(arguments.clone())?;
        Ok(input
            .resource
            .into_iter()
            .map(|resource| ResourceClaim::new(self.resource_action.clone(), resource))
            .collect())
    }

    async fn invoke(
        &self,
        arguments: serde_json::Value,
        authorizer: &dyn ResourceAuthorizer,
        cx: &ExecutionContext,
    ) -> Result<ToolOutcome> {
        let input = self.parse(arguments)?;

        // Stands in for a resource a real tool would only learn about mid-run: the target
        // of a symlink it just resolved, an entry found while walking a directory. It must
        // be authorized before being acted on, and a refusal must abort the call.
        if let Some(discovered) = &input.discovered_resource {
            authorizer
                .authorize(&self.resource_action, &discovered.as_str().into())
                .await?;
        }

        let mut touched: Vec<String> = Vec::new();
        touched.extend(input.resource.clone());
        touched.extend(input.discovered_resource.clone());

        let output = json!({ "text": input.text, "resources": touched });

        if input.delay_ms == 0 {
            return Ok(ToolOutcome::ok(output));
        }

        // The budget remaining under `cx`'s own deadline, computed against real wall-clock
        // time — this tool has no injected `Clock`, since it does not need one for
        // anything but this demonstration.
        let budget = cx.deadline.map(|deadline| {
            deadline
                .to_system_time()
                .duration_since(SystemTime::now())
                .unwrap_or_default()
        });

        tokio::select! {
            biased;
            () = cx.cancelled() => Err(Error::Cancelled),
            () = wait(budget) => Err(Error::Timeout(budget.unwrap_or_default())),
            () = tokio::time::sleep(Duration::from_millis(input.delay_ms)) => {
                Ok(ToolOutcome::ok(output))
            }
        }
    }
}

async fn wait(budget: Option<Duration>) {
    match budget {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::ResourceId;
    use aik_core::clock::Timestamp;
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    /// Records what it was asked, and answers with a fixed verdict.
    struct RecordingAuthorizer {
        allow: bool,
        asked: Mutex<Vec<(ActionId, ResourceId)>>,
    }

    impl RecordingAuthorizer {
        fn new(allow: bool) -> Self {
            Self {
                allow,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ResourceAuthorizer for RecordingAuthorizer {
        async fn authorize(&self, action: &ActionId, resource: &ResourceId) -> Result<()> {
            self.asked
                .lock()
                .unwrap()
                .push((action.clone(), resource.clone()));
            if self.allow {
                Ok(())
            } else {
                Err(Error::PermissionDenied("refused".into()))
            }
        }
    }

    fn allow() -> RecordingAuthorizer {
        RecordingAuthorizer::new(true)
    }

    #[tokio::test]
    async fn echoes_the_given_text() {
        let outcome = EchoTool::new()
            .invoke(json!({ "text": "hi" }), &allow(), &ExecutionContext::new())
            .await
            .unwrap();
        assert_eq!(outcome.output["text"], json!("hi"));
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn invalid_arguments_are_a_structured_error() {
        let error = EchoTool::new()
            .invoke(
                json!({ "delay_ms": "not a number" }),
                &allow(),
                &ExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn a_declared_resource_becomes_a_planned_claim() {
        let claims = EchoTool::new()
            .planned_resources(&json!({ "text": "hi", "resource": "/tmp/a" }))
            .unwrap();
        assert_eq!(
            claims,
            vec![ResourceClaim::new(
                ActionId::new(DEFAULT_PERMISSION),
                ResourceId::new("/tmp/a")
            )]
        );
    }

    #[test]
    fn no_declared_resource_means_no_claims() {
        let claims = EchoTool::new()
            .planned_resources(&json!({ "text": "hi" }))
            .unwrap();
        assert!(claims.is_empty());
    }

    #[tokio::test]
    async fn a_discovered_resource_is_authorized_before_use() {
        let authorizer = allow();
        EchoTool::new()
            .invoke(
                json!({ "text": "hi", "discovered_resource": "/tmp/found" }),
                &authorizer,
                &ExecutionContext::new(),
            )
            .await
            .unwrap();

        let asked = authorizer.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1, ResourceId::new("/tmp/found"));
    }

    #[tokio::test]
    async fn a_refused_discovered_resource_aborts_the_call() {
        let error = EchoTool::new()
            .invoke(
                json!({ "text": "hi", "discovered_resource": "/etc/shadow" }),
                &RecordingAuthorizer::new(false),
                &ExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
    }

    #[tokio::test]
    async fn cancelling_during_a_delay_stops_it_promptly() {
        let cx = ExecutionContext::new();
        let cancellation = cx.cancellation.clone();

        tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
            cancellation.cancel();
        });

        let started = tokio::time::Instant::now();
        let error = EchoTool::new()
            .invoke(json!({ "text": "hi", "delay_ms": 30_000 }), &allow(), &cx)
            .await
            .unwrap_err();

        assert!(matches!(error, Error::Cancelled), "{error}");
        assert!(started.elapsed() < StdDuration::from_secs(5));
    }

    #[tokio::test]
    async fn a_deadline_shorter_than_the_delay_times_out() {
        let deadline = Timestamp::from_millis(
            (SystemTime::now() + StdDuration::from_millis(20))
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        );
        let cx = ExecutionContext::new().with_deadline(deadline);

        let started = tokio::time::Instant::now();
        let error = EchoTool::new()
            .invoke(json!({ "text": "hi", "delay_ms": 30_000 }), &allow(), &cx)
            .await
            .unwrap_err();

        assert!(matches!(error, Error::Timeout(_)), "{error}");
        assert!(started.elapsed() < StdDuration::from_secs(5));
    }

    #[tokio::test]
    async fn a_deadline_longer_than_the_delay_does_not_interfere() {
        let deadline = Timestamp::from_millis(
            (SystemTime::now() + StdDuration::from_secs(30))
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        );
        let cx = ExecutionContext::new().with_deadline(deadline);

        let outcome = EchoTool::new()
            .invoke(json!({ "text": "hi", "delay_ms": 10 }), &allow(), &cx)
            .await
            .unwrap();
        assert_eq!(outcome.output["text"], json!("hi"));
    }
}
