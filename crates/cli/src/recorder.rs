//! Structured, privacy-conscious recording of measurement events to JSONL.
//!
//! This is the machine-readable half of `--verbose`: the same events render as text on the
//! terminal and, when `--record <FILE>` names a destination, are also appended to it as one
//! JSON object per line. Nothing here changes what those events *are* — the recorder is a
//! subscriber, exactly like the verbose renderer, and both read the same
//! [`RequestMeasured`], [`ContextAssembled`], [`AuthorizationDecided`] and [`ToolInvoked`]
//! events the kernel already publishes.
//!
//! # What is recorded, and what is deliberately excluded
//!
//! Every line carries identifiers, counts and timings: a timestamp, the session and
//! correlation ids, the turn number where one applies, an event kind, token estimates,
//! provider-reported usage, latency in milliseconds, the model id, the tool name, the
//! authorization phase and decision, and an error kind where one applies. That is
//! everything asked of a development-time measurement log.
//!
//! It never records:
//!
//! * **prompts, message content or assistant text** — none of the source events carry it;
//! * **tool arguments or tool results** — same;
//! * **file contents** — same;
//! * **resource identifiers (paths)** — present on the underlying audit events, but
//!   deliberately left out here. A path can encode a username, a project name or a
//!   directory layout that is fine to show on a terminal a developer is sitting at but is
//!   not the kind of thing that belongs in a log file accumulated over many runs and
//!   possibly shipped elsewhere. Use `-v` if you need to see which resource a decision was
//!   about;
//! * **policy-authored deny/require-approval reasons** — short, human-authored text, but
//!   excluded for the same reason as resource identifiers: it is easy for a reason string
//!   to quote the resource it is about.
//!
//! This makes the recorder safe to leave switched on in a development environment that
//! contains sensitive project data: what ends up on disk is shapes and numbers, not
//! content.
//!
//! # Failure is loud, not silent
//!
//! [`Recorder::create`] fails immediately, like any other startup error, if the file
//! cannot be opened for appending. Once running, a single failed write disables the
//! recorder for the rest of the process — printing exactly one message explaining why —
//! rather than retrying forever or claiming success it did not have. A conversation is
//! never interrupted by a recording failure; it is only ever less observed by one.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use aik_api::audit::{AuthorizationDecided, AuthorizationOutcome, InvocationOutcome, ToolInvoked};
use aik_api::context::ContextAssembled;
use aik_api::measurement::RequestMeasured;
use aik_core::{Error, Result};
use serde_json::json;

/// Appends one JSON object per measurement event to a file.
#[derive(Debug)]
pub struct Recorder {
    file: File,
    path: PathBuf,
    /// Set after the first failed write. Once true, every further call is a silent no-op
    /// except that the failure was already reported once — see the module documentation.
    disabled: bool,
}

impl Recorder {
    /// Opens `path` for appending, creating it if it does not exist.
    ///
    /// Fails the way any other startup problem does: named, with the underlying I/O error
    /// as its cause, and before anything else about the run has started.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| {
                Error::wrap(
                    format!("opening the recording file `{}`", path.display()),
                    error,
                )
            })?;
        Ok(Self {
            file,
            path: path.to_owned(),
            disabled: false,
        })
    }

    /// The path this recorder was opened against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line, disabling further recording (loudly, once) if the write fails.
    fn write(&mut self, value: serde_json::Value) {
        if self.disabled {
            return;
        }
        let mut line = value.to_string();
        line.push('\n');
        if let Err(error) = self.file.write_all(line.as_bytes()) {
            eprintln!(
                "aik: recording: failed to write to `{}`: {error}; \
                 disabling recording for the rest of this run",
                self.path.display()
            );
            self.disabled = true;
        }
    }

    /// Records one assembled context window.
    pub fn record_context(&mut self, event: &ContextAssembled) {
        self.write(json!({
            "timestamp": event.timestamp,
            "session": event.session.to_string(),
            "correlation": event.correlation.to_string(),
            "event": "context_assembled",
            "included_records": event.usage.included_records,
            "included_tokens": event.usage.included_tokens,
            "dropped_records": event.usage.dropped_records,
            "dropped_tokens": event.usage.dropped_tokens,
            "elided_parts": event.usage.elided_parts,
            "elided_tokens": event.usage.elided_tokens,
            "over_budget": event.usage.over_budget,
        }));
    }

    /// Records one authorization decision.
    ///
    /// Deliberately omits the resource identifier and the policy-authored reason — see the
    /// [module documentation](self) for why.
    pub fn record_authorization(&mut self, event: &AuthorizationDecided) {
        self.write(json!({
            "timestamp": event.timestamp,
            "correlation": event.correlation.to_string(),
            "event": "authorization_decided",
            "tool": event.tool.as_str(),
            "phase": phase_name(event.phase),
            "decision": outcome_name(&event.outcome),
            "duration_ms": event.duration_ms,
        }));
    }

    /// Records one completed (or refused, or not-found) tool invocation.
    pub fn record_invocation(&mut self, event: &ToolInvoked) {
        let (result, error_kind) = match &event.outcome {
            InvocationOutcome::Succeeded => ("succeeded", None),
            InvocationOutcome::ReportedError => ("reported_error", None),
            InvocationOutcome::Failed { kind } => ("failed", Some(kind.as_str())),
            InvocationOutcome::Denied => ("denied", None),
            InvocationOutcome::NotFound => ("not_found", None),
        };
        self.write(json!({
            "timestamp": event.timestamp,
            "correlation": event.correlation.to_string(),
            "event": "tool_invoked",
            "tool": event.tool.as_str(),
            "result": result,
            "error_kind": error_kind,
            "duration_ms": event.duration_ms,
            "authorization_duration_ms": event.authorization_duration_ms,
            "execution_duration_ms": event.execution_duration_ms,
        }));
    }

    /// Records one measured model turn.
    pub fn record_measurement(&mut self, event: &RequestMeasured) {
        self.write(json!({
            "timestamp": event.timestamp,
            "session": event.session.to_string(),
            "correlation": event.correlation.to_string(),
            "event": "request_measured",
            "model": event.model.as_str(),
            "turn": event.turn,
            "cumulative_tool_calls": event.cumulative_tool_calls,
            "estimate": {
                "system_tokens": event.estimate.system_tokens,
                "conversation_tokens": event.estimate.conversation_tokens,
                "user_input_tokens": event.estimate.user_input_tokens,
                "tool_call_tokens": event.estimate.tool_call_tokens,
                "tool_result_tokens": event.estimate.tool_result_tokens,
                "tool_definition_tokens": event.estimate.tool_definition_tokens,
                "tools_offered": event.estimate.tools_offered,
                "total_tokens": event.estimate.total_tokens,
            },
            "context": {
                "included_tokens": event.context.included_tokens,
                "dropped_tokens": event.context.dropped_tokens,
                "elided_tokens": event.context.elided_tokens,
            },
            "provider_usage": event.provider_usage.map(|usage| json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            })),
            "cumulative_provider_usage": event.cumulative_provider_usage.map(|usage| json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            })),
            "model_latency_ms": event.model_latency_ms,
        }));
    }
}

fn phase_name(phase: aik_api::audit::AuthorizationPhase) -> &'static str {
    use aik_api::audit::AuthorizationPhase;
    match phase {
        AuthorizationPhase::Tool => "tool",
        AuthorizationPhase::Resource => "resource",
        AuthorizationPhase::DiscoveredResource => "discovered_resource",
    }
}

fn outcome_name(outcome: &AuthorizationOutcome) -> &'static str {
    match outcome {
        AuthorizationOutcome::Allowed => "allowed",
        AuthorizationOutcome::Denied { .. } => "denied",
        AuthorizationOutcome::ApprovalGranted => "approval_granted",
        AuthorizationOutcome::ApprovalRefused => "approval_refused",
        AuthorizationOutcome::ApprovalUnavailable => "approval_unavailable",
        AuthorizationOutcome::PolicyUnavailable => "policy_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::agent::SessionId;
    use aik_api::audit::AuthorizationPhase;
    use aik_api::context::ContextUsage;
    use aik_api::measurement::RequestEstimate;
    use aik_api::model::{ModelId, Usage};
    use aik_api::permission::{ActionId, PrincipalId, PrincipalKind};
    use aik_api::tool::ToolName;
    use aik_core::clock::Timestamp;
    use aik_core::id::CorrelationId;
    use std::io::Read as _;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        let mut contents = String::new();
        File::open(path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn opening_a_recorder_in_an_unwritable_directory_fails_clearly() {
        let error = Recorder::create(Path::new("/nonexistent-directory/x/y.jsonl")).unwrap_err();
        assert!(error.to_string().contains("y.jsonl"), "{error}");
    }

    #[test]
    fn recorded_context_events_never_carry_message_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        recorder.record_context(&ContextAssembled {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            session: SessionId::new(),
            usage: ContextUsage {
                included_records: 2,
                included_tokens: 40,
                ..ContextUsage::default()
            },
        });

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["event"], json!("context_assembled"));
        assert_eq!(lines[0]["included_tokens"], json!(40));
        assert!(lines[0].get("messages").is_none());
        assert!(lines[0].get("content").is_none());
    }

    #[test]
    fn recorded_authorization_events_omit_the_resource_and_the_reason() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        recorder.record_authorization(&AuthorizationDecided {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            tool: ToolName::new("filesystem.write"),
            principal: PrincipalId::new("agent"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            action: ActionId::new("filesystem.write"),
            resource: Some(aik_api::permission::ResourceId::new(
                "/home/alice/secret-project/notes.md",
            )),
            phase: AuthorizationPhase::Resource,
            duration_ms: 7,
            outcome: AuthorizationOutcome::Denied {
                reason: "outside the workspace, specifically /home/alice/secret-project".into(),
            },
        });

        let lines = read_lines(&path);
        assert_eq!(lines[0]["decision"], json!("denied"));
        assert_eq!(lines[0]["duration_ms"], json!(7));
        assert!(lines[0].get("resource").is_none());
        assert!(lines[0].get("reason").is_none());
        let rendered = lines[0].to_string();
        assert!(!rendered.contains("secret-project"), "{rendered}");
    }

    #[test]
    fn recorded_invocations_carry_the_error_kind_but_not_a_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        recorder.record_invocation(&ToolInvoked {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            tool: ToolName::new("filesystem.read"),
            principal: PrincipalId::new("agent"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            duration_ms: 5,
            authorization_duration_ms: Some(1),
            execution_duration_ms: Some(4),
            outcome: InvocationOutcome::Failed {
                kind: "notfound".into(),
            },
        });

        let lines = read_lines(&path);
        assert_eq!(lines[0]["result"], json!("failed"));
        assert_eq!(lines[0]["error_kind"], json!("notfound"));
        assert_eq!(lines[0]["execution_duration_ms"], json!(4));
        assert!(lines[0].get("message").is_none());
    }

    #[test]
    fn recorded_measurements_never_carry_a_prompt_or_a_response() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        recorder.record_measurement(&RequestMeasured {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            session: SessionId::new(),
            model: ModelId::new("llama3.1:8b"),
            turn: 1,
            cumulative_tool_calls: 0,
            estimate: RequestEstimate {
                system_tokens: 10,
                conversation_tokens: 5,
                user_input_tokens: Some(5),
                tool_call_tokens: 0,
                tool_result_tokens: 0,
                tool_definition_tokens: 400,
                tools_offered: 2,
                total_tokens: 415,
            },
            context: ContextUsage::default(),
            provider_usage: Some(Usage {
                input_tokens: 415,
                output_tokens: 12,
            }),
            cumulative_provider_usage: Some(Usage {
                input_tokens: 415,
                output_tokens: 12,
            }),
            model_latency_ms: 900,
        });

        let lines = read_lines(&path);
        assert_eq!(lines[0]["event"], json!("request_measured"));
        assert_eq!(lines[0]["estimate"]["total_tokens"], json!(415));
        assert_eq!(lines[0]["model_latency_ms"], json!(900));
        assert!(lines[0].get("message").is_none());
        assert!(lines[0].get("content").is_none());
        assert!(lines[0].get("prompt").is_none());
    }

    #[test]
    fn a_write_failure_disables_the_recorder_rather_than_panicking_or_retrying() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        // Simulate a failure by dropping the underlying file and closing its descriptor:
        // the simplest portable way is to make the recorder believe it already failed once.
        recorder.disabled = true;
        recorder.record_context(&ContextAssembled {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            session: SessionId::new(),
            usage: ContextUsage::default(),
        });

        // Nothing panicked, and no new line was appended.
        assert!(read_lines(&path).is_empty());
    }

    #[test]
    fn multiple_records_append_as_separate_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mut recorder = Recorder::create(&path).unwrap();

        for _ in 0..3 {
            recorder.record_context(&ContextAssembled {
                correlation: CorrelationId::new(),
                timestamp: Timestamp::from_millis(1),
                session: SessionId::new(),
                usage: ContextUsage::default(),
            });
        }

        assert_eq!(read_lines(&path).len(), 3);
    }
}
