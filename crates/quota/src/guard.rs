//! [`LimitedQuotaGuard`]: the ceilings, applied.

use std::collections::HashMap;
use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::model::ModelId;
use aik_api::permission::{Principal, PrincipalId};
use aik_api::quota::{QuotaDimension, QuotaGuard, QuotaStatus, UsageCharge};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, Result};
use async_trait::async_trait;
use chrono::DateTime;

use crate::document::{QuotaDocument, QuotaRule};
use crate::ledger::{Counters, UsageLedger};
use crate::period::{QuotaPeriod, Window};

/// A [`QuotaGuard`] that enforces a [`QuotaDocument`] against a [`UsageLedger`].
///
/// # Who is charged
///
/// Both identities in play. A charge from a principal acting for somebody else is added to
/// the actor's counters *and* to the counters of whoever it acts for, as two independent
/// rows. That is what makes the two obvious ceilings mean what an operator expects: a rule
/// about `alice` holds however many agents do Alice's work, and a rule about `scheduler`
/// holds across everybody whose jobs it is running. Neither is a double charge — they are
/// two different questions, counted separately, and a deployment that only writes one of the
/// two rules only ever pays for one.
///
/// Every identity comes from the [`ExecutionContext`]. Nothing a model emits reaches this.
///
/// # Every matching rule applies
///
/// See [`QuotaDocument`]. A check refuses as soon as any applicable ceiling is reached, and
/// the refusal names which one, so an operator is never left to work out which of several
/// rules stopped a run.
///
/// # Failing closed
///
/// A ledger that cannot be read, a clock that reports an impossible instant, a model that
/// cannot be priced when a cost ceiling applies: each is a refusal. The reasoning is the same
/// in all three cases — the guard exists to answer "is there budget left?", and an
/// implementation that answers "probably" when it does not know is not a limit.
pub struct LimitedQuotaGuard {
    document: Arc<QuotaDocument>,
    ledger: Arc<dyn UsageLedger>,
    clock: SharedClock,
}

impl std::fmt::Debug for LimitedQuotaGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimitedQuotaGuard")
            .field("rules", &self.document.limits.len())
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl LimitedQuotaGuard {
    /// Builds a guard from a document and a ledger, validating the document first.
    pub fn new(document: QuotaDocument, ledger: Arc<dyn UsageLedger>) -> Result<Self> {
        document.validate()?;
        Ok(Self {
            document: Arc::new(document),
            ledger,
            clock: Arc::new(SystemClock),
        })
    }

    /// Overrides the clock windows are derived from. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// The document being enforced.
    pub fn document(&self) -> &QuotaDocument {
        &self.document
    }

    /// The identities one principal's usage is charged to.
    ///
    /// The actor, and whoever it is acting for. Deduplicated, because a principal acting for
    /// itself is one identity and would otherwise be charged twice for one turn.
    fn subjects(principal: &Principal) -> Vec<PrincipalId> {
        let mut subjects = vec![principal.id.clone()];
        if let Some(owner) = &principal.on_behalf_of
            && owner != &principal.id
        {
            subjects.push(owner.clone());
        }
        subjects
    }

    /// Every rule that counts `subject`.
    fn rules_for<'a>(&'a self, subject: &'a PrincipalId) -> impl Iterator<Item = &'a QuotaRule> {
        self.document
            .limits
            .iter()
            .filter(move |rule| rule.subject.matches(subject.as_str()))
    }

    /// The distinct periods `subject` is counted over, so a charge writes one row per period
    /// rather than one per rule.
    fn periods_for(&self, subject: &PrincipalId) -> Vec<QuotaPeriod> {
        let mut periods: Vec<QuotaPeriod> =
            self.rules_for(subject).map(|rule| rule.period).collect();
        periods.sort_unstable();
        periods.dedup();
        periods
    }
}

/// Reads each (subject, window) row at most once per call.
#[derive(Default)]
struct Rows {
    seen: HashMap<(PrincipalId, String), Counters>,
}

impl Rows {
    async fn get(
        &mut self,
        ledger: &dyn UsageLedger,
        subject: &PrincipalId,
        window: &str,
    ) -> Result<Counters> {
        let key = (subject.clone(), window.to_owned());
        if let Some(counters) = self.seen.get(&key) {
            return Ok(*counters);
        }
        let counters = ledger.read(subject.as_str(), window).await?;
        self.seen.insert(key, counters);
        Ok(counters)
    }
}

#[async_trait]
impl QuotaGuard for LimitedQuotaGuard {
    async fn check(&self, model: &ModelId, cx: &ExecutionContext) -> Result<()> {
        if self.document.is_empty() {
            return Ok(());
        }
        let principal = cx.principal_or_system();
        let now = self.clock.now();
        let mut rows = Rows::default();

        for subject in Self::subjects(&principal) {
            for rule in self.rules_for(&subject) {
                // Before the ledger is even read: a deployment that caps spend on a model it
                // never priced has written down something it cannot enforce, and taking the
                // turn anyway would charge it zero for ever.
                if rule.max_cost_micros.is_some() && self.document.price(model.as_str()).is_none() {
                    return Err(Error::PermissionDenied(format!(
                        "`{subject}` has a cost ceiling but `{model}` has no price, so what a \
                         turn would cost is unknown; add it under `prices` (a model that is \
                         genuinely free is priced at zero)"
                    )));
                }

                let window = rule.period.window(now)?;
                let counters = rows
                    .get(self.ledger.as_ref(), &subject, &window.key)
                    .await?;
                for (dimension, limit) in rule.ceilings() {
                    let used = counters.get(dimension);
                    if used >= limit {
                        return Err(Error::PermissionDenied(exhausted(
                            &subject, rule, &window, dimension, used, limit,
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    async fn record(&self, charge: &UsageCharge, cx: &ExecutionContext) -> Result<()> {
        if self.document.is_empty() {
            return Ok(());
        }
        let principal = cx.principal_or_system();
        let now = self.clock.now();

        // Priced at the moment it is charged. A price added to the configuration later does
        // not reprice what is already counted: a ledger row is what a period has spent, not
        // a transcript to be re-evaluated.
        let delta = Counters {
            turns: charge.turns,
            input_tokens: charge.input_tokens,
            output_tokens: charge.output_tokens,
            cost_micros: self
                .document
                .price(charge.model.as_str())
                .map_or(0, |price| price.cost_micros(charge)),
        };

        for subject in Self::subjects(&principal) {
            for period in self.periods_for(&subject) {
                let window = period.window(now)?;
                self.ledger
                    .add(subject.as_str(), period, &window.key, delta)
                    .await?;
            }
        }
        Ok(())
    }

    async fn status(&self, cx: &ExecutionContext) -> Result<Vec<QuotaStatus>> {
        if self.document.is_empty() {
            return Ok(Vec::new());
        }
        let principal = cx.principal_or_system();
        let now = self.clock.now();
        let mut rows = Rows::default();
        let mut statuses = Vec::new();

        for subject in Self::subjects(&principal) {
            for rule in self.rules_for(&subject) {
                let window = rule.period.window(now)?;
                let counters = rows
                    .get(self.ledger.as_ref(), &subject, &window.key)
                    .await?;
                for (dimension, limit) in rule.ceilings() {
                    statuses.push(QuotaStatus {
                        subject: subject.clone(),
                        window: window.key.clone(),
                        dimension,
                        used: counters.get(dimension),
                        limit,
                        resets_at: window.ends,
                    });
                }
            }
        }
        Ok(statuses)
    }
}

/// The refusal an exhausted ceiling produces.
///
/// It names the subject, the dimension, both numbers, the window and when the window closes,
/// because every one of those is something the person who hit it has to know in order to
/// decide whether to wait, to raise the ceiling, or to look for what spent the budget.
fn exhausted(
    subject: &PrincipalId,
    rule: &QuotaRule,
    window: &Window,
    dimension: QuotaDimension,
    used: u64,
    limit: u64,
) -> String {
    let mut message = format!(
        "`{subject}` has used {used} of {limit} {dimension} for this {}",
        rule.period
    );
    if let Some(description) = &rule.description {
        message.push_str(&format!(" ({description})"));
    }
    match window.ends {
        Some(ends) => message.push_str(&format!(
            "; the {} window `{}` resets at {}",
            rule.period,
            window.key,
            format_instant(ends)
        )),
        None => message.push_str("; this ceiling is cumulative and does not reset"),
    }
    message
}

/// Formats an instant as UTC, falling back to raw milliseconds for one that is not
/// representable — which cannot happen for a window boundary, since deriving one already
/// required the conversion to succeed.
fn format_instant(at: Timestamp) -> String {
    i64::try_from(at.as_millis())
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .map_or_else(
            || format!("{}ms since the epoch", at.as_millis()),
            |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ModelPrice;
    use crate::ledger::InMemoryUsageLedger;
    use aik_api::permission::PrincipalKind;
    use aik_core::ErrorKind;
    use aik_core::clock::ManualClock;

    /// 2026-08-28T14:35:12Z.
    const FRIDAY: u64 = 1_787_927_712_000;

    fn guard(
        document: QuotaDocument,
    ) -> (
        Arc<InMemoryUsageLedger>,
        Arc<ManualClock>,
        LimitedQuotaGuard,
    ) {
        let ledger = Arc::new(InMemoryUsageLedger::new());
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(FRIDAY)));
        let guard = LimitedQuotaGuard::new(document, ledger.clone())
            .unwrap()
            .with_clock(clock.clone());
        (ledger, clock, guard)
    }

    fn alice() -> ExecutionContext {
        ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
    }

    fn assistant_for_alice() -> ExecutionContext {
        ExecutionContext::new()
            .with_principal(Principal::new("assistant", PrincipalKind::Agent).on_behalf_of("alice"))
    }

    fn model() -> ModelId {
        ModelId::new("llama3.1:8b")
    }

    fn turns_rule(subject: &str, period: QuotaPeriod, max: u64) -> QuotaRule {
        QuotaRule::turns(subject, period, max)
    }

    fn document(rules: Vec<QuotaRule>) -> QuotaDocument {
        QuotaDocument {
            limits: rules,
            ..QuotaDocument::empty()
        }
    }

    #[tokio::test]
    async fn an_empty_document_never_refuses_and_never_writes() {
        let (ledger, _clock, guard) = guard(QuotaDocument::empty());
        guard.check(&model(), &alice()).await.unwrap();
        guard
            .record(&UsageCharge::turn(model(), 100, 20), &alice())
            .await
            .unwrap();
        assert_eq!(
            ledger.row_count(),
            0,
            "an unconfigured quota must cost nothing"
        );
        assert!(guard.status(&alice()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_ceiling_refuses_once_it_is_reached_and_not_before() {
        let (_ledger, _clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Day, 2)]));
        for _ in 0..2 {
            guard.check(&model(), &alice()).await.unwrap();
            guard
                .record(&UsageCharge::turn(model(), 10, 5), &alice())
                .await
                .unwrap();
        }

        let error = guard.check(&model(), &alice()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Permission);
        assert!(error.to_string().contains("alice"), "{error}");
        assert!(error.to_string().contains("2 of 2 model turns"), "{error}");
        assert!(error.to_string().contains("day:2026-08-28"), "{error}");
        assert!(
            error.to_string().contains("2026-08-29T00:00:00Z"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_counter_starts_again_when_its_window_closes() {
        let (_ledger, clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Day, 1)]));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &alice())
            .await
            .unwrap();
        assert!(guard.check(&model(), &alice()).await.is_err());

        clock.set(Timestamp::from_millis(FRIDAY + 24 * 60 * 60 * 1_000));
        guard.check(&model(), &alice()).await.unwrap();
    }

    #[tokio::test]
    async fn a_cumulative_ceiling_never_starts_again() {
        let (_ledger, clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Total, 1)]));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &alice())
            .await
            .unwrap();
        clock.set(Timestamp::from_millis(FRIDAY + 400 * 24 * 60 * 60 * 1_000));

        let error = guard.check(&model(), &alice()).await.unwrap_err();
        assert!(error.to_string().contains("does not reset"), "{error}");
    }

    #[tokio::test]
    async fn delegated_work_is_charged_to_the_agent_and_to_the_person() {
        let (ledger, _clock, guard) = guard(document(vec![
            turns_rule("assistant", QuotaPeriod::Day, 10),
            turns_rule("alice", QuotaPeriod::Day, 1),
        ]));

        guard.check(&model(), &assistant_for_alice()).await.unwrap();
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &assistant_for_alice())
            .await
            .unwrap();

        assert_eq!(
            ledger
                .read("assistant", "day:2026-08-28")
                .await
                .unwrap()
                .turns,
            1
        );
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );

        // Alice's own ceiling is what stops the agent, even though the agent's is untouched.
        let error = guard
            .check(&model(), &assistant_for_alice())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("alice"), "{error}");
    }

    #[tokio::test]
    async fn a_principal_acting_for_itself_is_charged_once() {
        let (ledger, _clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Day, 10)]));
        let cx = ExecutionContext::new()
            .with_principal(Principal::new("alice", PrincipalKind::User).on_behalf_of("alice"));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &cx)
            .await
            .unwrap();
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );
        assert_eq!(ledger.row_count(), 1);
    }

    #[tokio::test]
    async fn a_context_with_no_principal_is_charged_to_the_system() {
        let (ledger, _clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Day, 10)]));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &ExecutionContext::new())
            .await
            .unwrap();
        assert_eq!(
            ledger.read("system", "day:2026-08-28").await.unwrap().turns,
            1
        );
    }

    #[tokio::test]
    async fn the_tightest_matching_rule_is_the_one_that_stops_a_run() {
        let (_ledger, _clock, guard) = guard(document(vec![
            turns_rule("*", QuotaPeriod::Day, 100),
            turns_rule("alice", QuotaPeriod::Day, 1),
        ]));
        guard.check(&model(), &alice()).await.unwrap();
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &alice())
            .await
            .unwrap();

        assert!(
            guard.check(&model(), &alice()).await.is_err(),
            "a broad rule must not widen a narrow one"
        );
        // Somebody the narrow rule does not name is still under the broad one only.
        let bob =
            ExecutionContext::new().with_principal(Principal::new("bob", PrincipalKind::User));
        guard.check(&model(), &bob).await.unwrap();
    }

    #[tokio::test]
    async fn one_charge_counts_against_every_period_that_is_capped() {
        let (ledger, _clock, guard) = guard(document(vec![
            turns_rule("*", QuotaPeriod::Day, 10),
            turns_rule("*", QuotaPeriod::Month, 100),
            turns_rule("*", QuotaPeriod::Total, 1_000),
        ]));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &alice())
            .await
            .unwrap();

        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );
        assert_eq!(
            ledger.read("alice", "month:2026-08").await.unwrap().turns,
            1
        );
        assert_eq!(ledger.read("alice", "total").await.unwrap().turns, 1);
        assert_eq!(
            ledger.row_count(),
            3,
            "one row per period, not one per rule"
        );
    }

    #[tokio::test]
    async fn token_ceilings_count_each_direction_and_the_sum() {
        let (_ledger, _clock, guard) = guard(document(vec![QuotaRule {
            max_turns: None,
            max_total_tokens: Some(100),
            ..turns_rule("*", QuotaPeriod::Day, 1)
        }]));
        guard
            .record(&UsageCharge::turn(model(), 60, 40), &alice())
            .await
            .unwrap();

        let error = guard.check(&model(), &alice()).await.unwrap_err();
        assert!(error.to_string().contains("100 of 100 tokens"), "{error}");
    }

    #[tokio::test]
    async fn cost_is_priced_from_the_tokens_that_were_actually_spent() {
        let mut document = document(vec![QuotaRule {
            max_turns: None,
            max_cost_micros: Some(1_000_000),
            ..turns_rule("*", QuotaPeriod::Month, 1)
        }]);
        document.prices.insert(
            "llama*".into(),
            ModelPrice {
                input_micros_per_million: 1_000_000,
                output_micros_per_million: 2_000_000,
            },
        );
        let (ledger, _clock, guard) = guard(document);

        // 400k in, 300k out: 0.40 + 0.60 = 1.00 currency units, exactly the ceiling.
        guard
            .record(&UsageCharge::turn(model(), 400_000, 300_000), &alice())
            .await
            .unwrap();
        assert_eq!(
            ledger
                .read("alice", "month:2026-08")
                .await
                .unwrap()
                .cost_micros,
            1_000_000
        );

        let error = guard.check(&model(), &alice()).await.unwrap_err();
        assert!(error.to_string().contains("cost (micros)"), "{error}");
    }

    #[tokio::test]
    async fn a_cost_ceiling_refuses_a_model_the_deployment_never_priced() {
        let mut document = document(vec![QuotaRule {
            max_turns: None,
            max_cost_micros: Some(1_000_000),
            ..turns_rule("*", QuotaPeriod::Month, 1)
        }]);
        document.prices.insert(
            "claude-*".into(),
            ModelPrice {
                input_micros_per_million: 3_000_000,
                output_micros_per_million: 15_000_000,
            },
        );
        let (_ledger, _clock, guard) = guard(document);

        let error = guard.check(&model(), &alice()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Permission);
        assert!(error.to_string().contains("no price"), "{error}");
        assert!(error.to_string().contains("llama3.1:8b"), "{error}");

        // The same deployment is fine with a model it did price.
        guard
            .check(&ModelId::new("claude-opus-5"), &alice())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_unpriced_model_is_allowed_when_nothing_caps_cost() {
        let (_ledger, _clock, guard) = guard(document(vec![turns_rule("*", QuotaPeriod::Day, 10)]));
        guard.check(&model(), &alice()).await.unwrap();
    }

    #[tokio::test]
    async fn status_reports_every_ceiling_that_applies() {
        let (_ledger, _clock, guard) = guard(document(vec![
            turns_rule("alice", QuotaPeriod::Day, 10),
            turns_rule("bob", QuotaPeriod::Day, 10),
        ]));
        guard
            .record(&UsageCharge::turn(model(), 1, 1), &alice())
            .await
            .unwrap();

        let status = guard.status(&alice()).await.unwrap();
        assert_eq!(
            status.len(),
            1,
            "a ceiling for somebody else is not Alice's"
        );
        assert_eq!(status[0].subject, PrincipalId::new("alice"));
        assert_eq!(status[0].dimension, QuotaDimension::Turns);
        assert_eq!(status[0].used, 1);
        assert_eq!(status[0].limit, 10);
        assert_eq!(status[0].remaining(), 9);
        assert!(status[0].resets_at.is_some());
    }

    #[tokio::test]
    async fn a_ledger_that_cannot_be_read_refuses_rather_than_allowing() {
        #[derive(Debug)]
        struct Broken;

        #[async_trait]
        impl UsageLedger for Broken {
            async fn read(&self, _: &str, _: &str) -> Result<Counters> {
                Err(Error::other("the disk is on fire"))
            }
            async fn add(&self, _: &str, _: QuotaPeriod, _: &str, _: Counters) -> Result<()> {
                Err(Error::other("the disk is on fire"))
            }
        }

        let guard = LimitedQuotaGuard::new(
            document(vec![turns_rule("*", QuotaPeriod::Day, 10)]),
            Arc::new(Broken),
        )
        .unwrap();
        assert!(guard.check(&model(), &alice()).await.is_err());
        assert!(
            guard
                .record(&UsageCharge::turn(model(), 1, 1), &alice())
                .await
                .is_err(),
            "a caller must find out that its spend went unrecorded"
        );
    }

    #[tokio::test]
    async fn an_invalid_document_is_refused_at_construction() {
        let error = LimitedQuotaGuard::new(
            document(vec![turns_rule("*", QuotaPeriod::Day, 0)]),
            Arc::new(InMemoryUsageLedger::new()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }
}
