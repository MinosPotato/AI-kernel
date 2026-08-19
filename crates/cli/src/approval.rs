//! Answering `require_approval` from the terminal.
//!
//! # What attaching means
//!
//! An [`ApprovalBroker`](aik_approval::ApprovalBroker) parks a question only while at least
//! one [`ApprovalGate`](aik_approval::ApprovalGate) exists; with none, it refuses
//! immediately. Holding a gate is therefore an assertion that a human will actually be
//! asked, and the frontend makes that assertion in exactly one place: an interactive
//! session subscribes, a one-shot run does not. Nothing else in the process can obtain a
//! gate, because obtaining one requires the broker, and the broker is held by the frontend
//! and published to components — never to an agent, which only ever sees a
//! [`ToolRegistry`](aik_api::tool::ToolRegistry).
//!
//! # What is shown
//!
//! The question, the action and the resource — all authored by the policy engine or the
//! registry. Deliberately *not* the tool's arguments: [`PendingApproval`] does not carry
//! them, and putting model-written bytes in front of somebody about to say "allow" is how
//! an approval prompt becomes an attack surface. Everything printed is still passed through
//! [`render::safe`](crate::render::safe), because a resource id is a path and a path can
//! contain anything.

use aik_approval::{ApprovalStream, PendingApproval};
use aik_core::Result;
use tokio::io::AsyncBufRead;

use crate::console::Console;
use crate::render::safe;

/// How an answer is interpreted.
///
/// Anything that is not an unambiguous yes is a no. There is no default-allow reading of a
/// blank line, a typo, or a closed input: the mechanism exists to stop things happening
/// that nobody agreed to.
pub fn granted(answer: Option<&str>) -> bool {
    matches!(
        answer
            .map(|text| text.trim().to_ascii_lowercase())
            .as_deref(),
        Some("y" | "yes"),
    )
}

/// Renders one pending question.
pub fn question(pending: &PendingApproval) -> String {
    let resource = pending
        .request
        .resource
        .as_ref()
        .map(|resource| format!("\n    resource: {}", safe(resource.as_str())))
        .unwrap_or_default();

    format!(
        "\n  ⚠ {}\n    action:   {}{}\n    asked by: {} (for {})\n  allow? [y/N] ",
        safe(&pending.prompt),
        safe(pending.request.action.as_str()),
        resource,
        safe(pending.request.principal.id.as_str()),
        pending
            .request
            .principal
            .on_behalf_of
            .as_ref()
            .map_or_else(|| "nobody".to_owned(), |id| safe(id.as_str())),
    )
}

/// Puts one question to the person at the terminal and answers it through the gate.
///
/// A read failure or end of input denies, rather than propagating: the run should carry on
/// and be told "no", which is what happens when nobody answers anyway.
pub async fn answer<R: AsyncBufRead + Unpin + Send>(
    stream: &ApprovalStream,
    pending: &PendingApproval,
    console: &mut Console<R>,
) -> Result<()> {
    let reply = console.ask(&question(pending)).await.unwrap_or(None);
    let granted = granted(reply.as_deref());
    println!("  {}", if granted { "allowed" } else { "denied" });

    // A late answer is a `NotFound`: the requester already gave up, and nothing was
    // permitted. Not worth failing the session over.
    let result = if granted {
        stream.gate().approve(&pending.id)
    } else {
        stream.gate().deny(&pending.id)
    };
    if let Err(error) = result {
        println!("  (the request was no longer waiting: {error})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::permission::{ActionId, PermissionRequest, Principal, PrincipalKind, ResourceId};
    use aik_approval::ApprovalId;
    use aik_core::clock::Timestamp;
    use aik_core::id::CorrelationId;

    fn pending(prompt: &str, resource: Option<&str>) -> PendingApproval {
        PendingApproval {
            id: ApprovalId::new(),
            request: PermissionRequest {
                principal: Principal::new("assistant", PrincipalKind::Agent).on_behalf_of("alice"),
                action: ActionId::new("filesystem.write"),
                resource: resource.map(ResourceId::new),
                context: serde_json::Value::Null,
            },
            prompt: prompt.to_owned(),
            correlation: CorrelationId::new(),
            requested_at: Timestamp::from_millis(0),
            expires_at: Timestamp::from_millis(1_000),
        }
    }

    #[test]
    fn only_an_explicit_yes_grants() {
        for answer in ["y", "Y", "yes", "YES", " yes "] {
            assert!(granted(Some(answer)), "{answer:?} should grant");
        }
    }

    #[test]
    fn everything_else_refuses() {
        for answer in ["", " ", "n", "no", "sure", "ok", "allow", "1", "true", "yy"] {
            assert!(!granted(Some(answer)), "{answer:?} should refuse");
        }
    }

    #[test]
    fn no_answer_at_all_refuses() {
        // End of input: a piped session, or a terminal that went away mid-question.
        assert!(!granted(None));
    }

    #[test]
    fn the_question_shows_what_policy_wrote_and_who_is_asking() {
        let rendered = question(&pending(
            "let the agent edit this file?",
            Some("/tmp/a.txt"),
        ));
        assert!(
            rendered.contains("let the agent edit this file?"),
            "{rendered}"
        );
        assert!(rendered.contains("filesystem.write"), "{rendered}");
        assert!(rendered.contains("/tmp/a.txt"), "{rendered}");
        assert!(rendered.contains("assistant"), "{rendered}");
        assert!(rendered.contains("alice"), "{rendered}");
    }

    #[test]
    fn a_hostile_resource_path_cannot_repaint_the_prompt() {
        let rendered = question(&pending("edit?", Some("/tmp/\x1b[2Kallow? [Y/n] y")));
        assert!(!rendered.contains('\x1b'), "{rendered}");
        assert!(rendered.trim_end().ends_with("allow? [y/N]"), "{rendered}");
    }

    #[test]
    fn a_hostile_prompt_cannot_either() {
        // The prompt is policy-authored and therefore trusted, but a policy document is
        // still a file somebody edits, and sanitising it costs nothing.
        let rendered = question(&pending("edit\x1b[1;31m?", None));
        assert!(!rendered.contains('\x1b'), "{rendered}");
    }
}
