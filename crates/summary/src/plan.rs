//! Deciding what one round summarises, before anything is sent or removed.
//!
//! Kept separate from [`crate::summariser`] because it is the part with no I/O in it: given
//! a session's records and the budget its windows are assembled under, it is a pure function
//! from "what is stored" to "what this round covers". That is what makes the awkward cases
//! — a budget with room for nothing, one enormous record, a session barely over the line —
//! testable without a model, a store or a clock.
//!
//! The rule it exists to keep is one-directional: **a round may summarise fewer records than
//! it would like, and must never remove more than it summarised.** Everything here that
//! shrinks the victim set (a character cap, a token allowance, a floor on what is retained)
//! shrinks what is later removed by exactly the same amount, because the removal count is
//! taken from this plan and not recomputed.

use aik_api::context::{ContextBudget, ContextRecord};

use crate::excerpt;
use crate::settings::SummarySettings;

/// What one round will summarise, and what it will then reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// How many of the session's oldest unpinned records this round covers.
    pub(crate) summarised_records: usize,
    /// Their full-fidelity cost, which is what removing them reclaims.
    pub(crate) reclaimed_tokens: u64,
    /// How many pinned records the session holds, which removal must account for.
    pub(crate) pinned_records: usize,
    /// The rendered transcript, already wrapped in its delimiter.
    pub(crate) excerpt: String,
}

/// How many of the newest unpinned records a round leaves in place.
///
/// Three constraints, applied in the order they can shrink the number, then a floor:
///
/// 1. the configured record count, which is the whole answer when no budget applies;
/// 2. the budget's record cap, less whatever the pinned records already claim;
/// 3. the share of the token budget survivors are allowed to occupy — counted from the
///    newest backwards, so what is kept is the recent end of the conversation.
///
/// The floor is last and wins, because every constraint above it is about *cost* and the
/// floor is about the session still being a conversation afterwards.
fn retained(
    unpinned: &[&ContextRecord],
    pinned_records: usize,
    pinned_tokens: u64,
    budget: &ContextBudget,
    settings: &SummarySettings,
) -> usize {
    let mut keep = settings.keep_recent_records.min(unpinned.len());

    if let Some(max_records) = budget.max_records {
        keep = keep.min(max_records.saturating_sub(pinned_records));
    }

    if let Some(max_tokens) = budget.max_tokens {
        let allowance = max_tokens
            .saturating_sub(pinned_tokens)
            .saturating_mul(settings.retain_percent)
            / 100;
        let mut spent = 0u64;
        let mut fitting = 0usize;
        for record in unpinned.iter().rev() {
            spent = spent.saturating_add(record.tokens);
            if spent > allowance {
                break;
            }
            fitting += 1;
        }
        keep = keep.min(fitting);
    }

    keep.max(settings.min_retained_records).min(unpinned.len())
}

/// Plans a round over `records`, or `None` when there is nothing worth doing.
///
/// `records` must be the session's full transcript in append order. `None` means the session
/// is left exactly as it is: too short, too little of it evictable, or not enough of it
/// older than what the budget wants kept.
pub(crate) fn plan(
    records: &[ContextRecord],
    budget: &ContextBudget,
    settings: &SummarySettings,
) -> Option<Plan> {
    let pinned_records = records.iter().filter(|record| record.pinned).count();
    let pinned_tokens: u64 = records
        .iter()
        .filter(|record| record.pinned)
        .map(|record| record.tokens)
        .sum();
    let unpinned: Vec<&ContextRecord> = records.iter().filter(|record| !record.pinned).collect();

    let keep = retained(&unpinned, pinned_records, pinned_tokens, budget, settings);
    let candidates = &unpinned[..unpinned.len().saturating_sub(keep)];
    if candidates.len() < settings.min_summarised_records {
        return None;
    }

    // Rendered oldest first, stopping at the character cap. Stopping *early* is safe — the
    // records past the cap stay in the session and the next round covers them — whereas
    // rendering past it would send a model more transcript than the deployment agreed to
    // pay for. The minimum is honoured even so: a round that summarised nothing would leave
    // the session in exactly the state that triggered it, and it would be triggered again.
    let mut body = String::new();
    let mut summarised_records = 0usize;
    let mut reclaimed_tokens = 0u64;
    for record in candidates {
        let Some(rendered) = excerpt::render_record(record, settings.max_part_chars) else {
            // Nothing to summarise, but still a record this round accounts for: leaving it
            // behind would mean a session that can never shed it.
            summarised_records += 1;
            reclaimed_tokens = reclaimed_tokens.saturating_add(record.tokens);
            continue;
        };
        let would_be = body.len() + rendered.len() + 1;
        if would_be > settings.max_excerpt_chars
            && summarised_records >= settings.min_summarised_records
        {
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&rendered);
        summarised_records += 1;
        reclaimed_tokens = reclaimed_tokens.saturating_add(record.tokens);
    }

    if body.is_empty() {
        return None;
    }

    Some(Plan {
        summarised_records,
        reclaimed_tokens,
        pinned_records,
        excerpt: excerpt::wrap(&body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::agent::SessionId;
    use aik_api::context::ContextId;
    use aik_api::model::{ContentPart, Message, Role};
    use aik_api::permission::PrincipalId;
    use aik_core::clock::Timestamp;

    fn record(sequence: u64, tokens: u64, pinned: bool, text: &str) -> ContextRecord {
        ContextRecord {
            id: ContextId::new(),
            session: SessionId::new(),
            sequence,
            message: Message {
                role: Role::User,
                content: vec![ContentPart::text(text)],
                name: None,
            },
            pinned,
            principal: PrincipalId::new("p"),
            created_at: Timestamp::from_millis(sequence),
            tokens,
        }
    }

    fn conversation(turns: u64) -> Vec<ContextRecord> {
        (0..turns)
            .map(|index| record(index, 10, false, &format!("turn {index}")))
            .collect()
    }

    fn settings() -> SummarySettings {
        SummarySettings::new("small").keeping(4)
    }

    #[test]
    fn a_short_session_is_left_alone() {
        let records = conversation(5);
        assert!(plan(&records, &ContextBudget::UNLIMITED, &settings()).is_none());
    }

    #[test]
    fn the_oldest_records_past_the_kept_ones_are_what_a_round_covers() {
        let records = conversation(10);
        let plan = plan(&records, &ContextBudget::UNLIMITED, &settings()).expect("a plan");
        assert_eq!(plan.summarised_records, 6, "ten records, four kept");
        assert_eq!(plan.reclaimed_tokens, 60);
        assert!(plan.excerpt.contains("turn 0"));
        assert!(plan.excerpt.contains("turn 5"));
        assert!(!plan.excerpt.contains("turn 6"), "the newest four are kept");
    }

    #[test]
    fn pinned_records_are_neither_summarised_nor_counted_as_kept() {
        let mut records = vec![record(0, 10, true, "system instructions")];
        records.extend(conversation(10));
        let plan = plan(&records, &ContextBudget::UNLIMITED, &settings()).expect("a plan");
        assert_eq!(plan.pinned_records, 1);
        assert_eq!(
            plan.summarised_records, 6,
            "the pinned record is not a turn"
        );
        assert!(
            !plan.excerpt.contains("system instructions"),
            "a pinned record is never summarised away"
        );
    }

    #[test]
    fn a_token_budget_keeps_less_than_the_record_count_would() {
        let records = conversation(10);
        // Room for two records' worth of survivors: 40 tokens, half of which may be kept.
        let budget = ContextBudget::tokens(40);
        let plan = plan(&records, &budget, &settings()).expect("a plan");
        assert_eq!(plan.summarised_records, 8, "only two of ten survive");
    }

    #[test]
    fn pinned_records_are_charged_to_the_budget_before_survivors_are() {
        let mut records = vec![record(0, 30, true, "system")];
        records.extend(conversation(10));
        // 40 tokens, 30 of them already spent on the pinned record: five left to keep with,
        // which is not even one record.
        let plan = plan(&records, &ContextBudget::tokens(40), &settings()).expect("a plan");
        assert_eq!(
            plan.summarised_records, 8,
            "the floor keeps two records even when the budget has room for none"
        );
    }

    #[test]
    fn a_budget_with_room_for_nothing_still_leaves_a_conversation() {
        let records = conversation(10);
        let plan = plan(&records, &ContextBudget::tokens(1), &settings()).expect("a plan");
        assert_eq!(plan.summarised_records, 8);
        assert!(
            plan.excerpt.contains("turn 7"),
            "everything but the newest two is covered"
        );
        assert!(!plan.excerpt.contains("turn 8"), "the newest two survive");
    }

    #[test]
    fn a_record_cap_narrows_what_survives() {
        let records = conversation(10);
        let budget = ContextBudget::default().with_max_records(3);
        let plan = plan(&records, &budget, &settings()).expect("a plan");
        assert_eq!(plan.summarised_records, 7);
    }

    #[test]
    fn the_character_cap_covers_fewer_records_rather_than_sending_more() {
        let records: Vec<ContextRecord> = (0..10)
            .map(|index| record(index, 10, false, &"x".repeat(500)))
            .collect();
        let settings = settings().with_max_excerpt_chars(1_200);
        let plan = plan(&records, &ContextBudget::UNLIMITED, &settings).expect("a plan");
        assert!(
            plan.summarised_records < 6,
            "covered {} records",
            plan.summarised_records
        );
        assert_eq!(
            plan.reclaimed_tokens,
            plan.summarised_records as u64 * 10,
            "what is reclaimed is exactly what was covered"
        );
    }

    #[test]
    fn the_minimum_is_covered_even_when_one_record_exceeds_the_cap() {
        let records: Vec<ContextRecord> = (0..10)
            .map(|index| record(index, 10, false, &"x".repeat(5_000)))
            .collect();
        let settings = settings()
            .with_max_excerpt_chars(10)
            .with_max_part_chars(5_000);
        let plan = plan(&records, &ContextBudget::UNLIMITED, &settings).expect("a plan");
        assert_eq!(plan.summarised_records, 2, "the floor still makes progress");
    }

    #[test]
    fn the_excerpt_is_delimited() {
        let records = conversation(10);
        let plan = plan(&records, &ContextBudget::UNLIMITED, &settings()).expect("a plan");
        assert!(plan.excerpt.starts_with(excerpt::TRANSCRIPT_OPEN));
        assert!(plan.excerpt.ends_with(excerpt::TRANSCRIPT_CLOSE));
    }

    #[test]
    fn a_session_of_nothing_but_pinned_records_is_left_alone() {
        let records: Vec<ContextRecord> = (0..10)
            .map(|index| record(index, 10, true, "pinned"))
            .collect();
        assert!(plan(&records, &ContextBudget::UNLIMITED, &settings()).is_none());
    }
}
