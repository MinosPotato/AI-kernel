//! The [`ContextCompactor`] itself: read the oldest turns, have a model write down what they
//! amounted to, put that back, and only then reclaim.
//!
//! # The order of the four steps is the safety property
//!
//! Read, summarise, append, reclaim. Every failure stops the sequence where it happened, and
//! because removal is last, every one of them leaves the session holding more than it needs
//! rather than less than it had. A compactor that reclaimed first and summarised after would
//! turn a model outage into permanent, silent history loss — which is exactly the failure
//! this crate exists to prevent, arriving through the thing that was supposed to prevent it.
//!
//! # What it is trusted with, and what it is not
//!
//! It holds a [`ModelProvider`] and a [`ContextStore`], and it is reached only by trusted
//! code. It is not a [`Tool`](aik_api::tool::Tool) and must never be registered as one: a
//! model that could ask for its own history to be compacted could choose what its history
//! says.
//!
//! Everything it stores is model output, so it stores it the way the contract requires —
//! unpinned, never as a system message, and behind
//! [`SUMMARY_MARKER`]. See [`ContextCompactor`] for why each of those matters.

use std::sync::Arc;

use aik_api::agent::SessionId;
use aik_api::context::{
    Compaction, ContextBudget, ContextCompacted, ContextCompactor, ContextEntry, ContextRecord,
    ContextStore,
};
use aik_api::execution::ExecutionContext;
use aik_api::model::{CompletionRequest, ContentPart, FinishReason, Message, ModelProvider, Role};
use aik_core::clock::{SharedClock, SystemClock};
use aik_core::event::{Envelope, EventBus};
use aik_core::id::ComponentId;
use aik_core::{Error, Result};
use async_trait::async_trait;

use crate::plan::{self, Plan};
use crate::settings::SummarySettings;

/// Replaces a session's oldest turns with a model-written recap of them.
pub struct Summariser {
    models: Arc<dyn ModelProvider>,
    context: Arc<dyn ContextStore>,
    settings: SummarySettings,
    clock: SharedClock,
    events: Option<(EventBus, ComponentId)>,
}

impl std::fmt::Debug for Summariser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Summariser")
            .field("model", &self.settings.model)
            .field("keep_recent_records", &self.settings.keep_recent_records)
            .field("observable", &self.events.is_some())
            .finish_non_exhaustive()
    }
}

impl Summariser {
    /// Compacts `context` by asking `models` for a recap, under `settings`.
    pub fn new(
        models: Arc<dyn ModelProvider>,
        context: Arc<dyn ContextStore>,
        settings: SummarySettings,
    ) -> Self {
        Self {
            models,
            context,
            settings,
            clock: Arc::new(SystemClock),
            events: None,
        }
    }

    /// Overrides the clock used to stamp [`ContextCompacted`]. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Publishes [`ContextCompacted`] to the kernel event bus, attributed to `source`.
    ///
    /// Without a bus, compaction happens identically and simply is not observable — which
    /// is worth avoiding here more than elsewhere, since this is the one operation that
    /// makes part of a transcript unavailable in full.
    #[must_use]
    pub fn with_events(mut self, events: EventBus, source: ComponentId) -> Self {
        self.events = Some((events, source));
        self
    }

    /// Reads a session's full-fidelity transcript, oldest first.
    ///
    /// Assembled under [`ContextBudget::UNLIMITED`] to enumerate the records, then read one
    /// by one because a window carries neither
    /// [`ContextRecord::pinned`] nor [`ContextRecord::tokens`], and planning needs both. A
    /// record that assembly leaves out — one whose every part was a tool result stranded by
    /// an earlier round — is therefore not summarised; it is still reclaimed, because it is
    /// older than everything that is, and it carries nothing a recap could have said.
    async fn transcript(
        &self,
        session: &SessionId,
        cx: &ExecutionContext,
    ) -> Result<Vec<ContextRecord>> {
        let window = self
            .context
            .window(session, &ContextBudget::UNLIMITED, cx)
            .await?;
        let mut records = Vec::with_capacity(window.records.len());
        for id in window.records {
            if let Some(record) = self.context.get(session, &id, cx).await? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Asks the model for the recap, and refuses anything that is not one.
    ///
    /// The request carries no tools, deliberately: this call's input is untrusted text, and
    /// a call that offers no tools cannot be talked into making one however the transcript
    /// is worded. A provider that returns a tool call anyway is not obeyed — only text parts
    /// are read — because a `dyn ToolRegistry` is not something this crate holds.
    ///
    /// An empty answer is an error rather than an empty summary: the caller is about to
    /// remove the turns this was supposed to replace, and "the model said nothing" must not
    /// be the thing that stands in for them.
    async fn write_summary(&self, plan: &Plan, cx: &ExecutionContext) -> Result<String> {
        let request = CompletionRequest {
            model: self.settings.model.clone(),
            messages: vec![
                Message::text(Role::System, self.settings.instructions.clone()),
                Message::text(Role::User, plan.excerpt.clone()),
            ],
            tools: Vec::new(),
            parameters: self.settings.parameters.clone(),
        };

        let response = self.models.complete(request, &cx.child()).await?;
        if response.finish_reason == FinishReason::Cancelled {
            return Err(Error::Cancelled);
        }

        let text: String = response
            .message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::other(format!(
                "summarising {} records with `{}` produced no text, so there is nothing to \
                 put in their place",
                plan.summarised_records, self.settings.model
            )));
        }

        Ok(crate::mark(
            text,
            plan.summarised_records,
            self.settings.max_summary_chars,
        ))
    }

    /// Removes exactly the records the recap covers.
    ///
    /// The count is derived at this moment rather than carried from the plan, and that is
    /// what makes it safe against a turn appended while the model was writing:
    /// [`ContextStore::compact`] keeps the newest *n* unpinned records, so `n` is read from
    /// what the session holds now, less what this round covered. Anything appended in the
    /// meantime raises the session's count and the kept count together, and what is removed
    /// stays the oldest `summarised_records` — never a turn nobody has summarised.
    async fn reclaim(
        &self,
        session: &SessionId,
        plan: &Plan,
        cx: &ExecutionContext,
    ) -> Result<usize> {
        let Some(stats) = self.context.stats(session, cx).await? else {
            return Ok(0);
        };
        let unpinned = stats.records.saturating_sub(plan.pinned_records);
        let keep = unpinned.saturating_sub(plan.summarised_records);
        self.context.compact(session, keep, cx).await
    }

    /// Announces what a round did, if anyone is listening.
    fn report(&self, cx: &ExecutionContext, session: SessionId, compaction: Compaction) {
        let Some((bus, source)) = &self.events else {
            return;
        };
        let metadata = bus
            .metadata_for::<ContextCompacted>()
            .with_source(source.clone())
            .with_correlation(cx.correlation);
        bus.publish_envelope(Envelope::new(
            metadata,
            ContextCompacted {
                correlation: cx.correlation,
                timestamp: self.clock.now(),
                session,
                compaction,
            },
        ));
    }
}

#[async_trait]
impl ContextCompactor for Summariser {
    async fn compact(
        &self,
        session: &SessionId,
        budget: &ContextBudget,
        cx: &ExecutionContext,
    ) -> Result<Compaction> {
        let records = self.transcript(session, cx).await?;
        let Some(plan) = plan::plan(&records, budget, &self.settings) else {
            return Ok(Compaction::NONE);
        };

        let summary = self.write_summary(&plan, cx).await?;

        // Unpinned, and never a system message: this is model output going back into the
        // conversation it came from. See `ContextCompactor` for what each of those rules
        // rules out.
        let record = self
            .context
            .append(
                session,
                ContextEntry::new(Message::text(Role::User, summary)),
                cx,
            )
            .await?;

        let removed = self.reclaim(session, &plan, cx).await?;
        let compaction = Compaction {
            summary: Some(record.id),
            summarised_records: plan.summarised_records,
            removed_records: removed,
            reclaimed_tokens: plan.reclaimed_tokens,
            summary_tokens: record.tokens,
        };

        tracing::debug!(
            session = %session,
            summarised = compaction.summarised_records,
            removed = compaction.removed_records,
            saved = compaction.saved_tokens(),
            "compacted a session"
        );
        self.report(cx, *session, compaction);
        Ok(compaction)
    }
}
